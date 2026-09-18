import { useEffect, useMemo, useState } from 'react'
import { useMutation, useQuery } from '@tanstack/react-query'
import { api } from '../../api/client'
import type { QueryFilter, QueryRequest, QueryResponse } from '../../api/types'
import { formatMicros, numberFormat } from '../../components/format'
import { ResultTable } from '../../components/ResultTable'
import { Notice, PageHeader, Panel } from '../../components/ui'

type DraftFilter = { id: number; column: string; op: 'eq' | 'in' | 'range'; value: string; upper: string; isNull: boolean }
let filterId = 1

function newFilter(column = ''): DraftFilter {
  return { id: filterId++, column, op: 'eq', value: '', upper: '', isNull: false }
}

export function QueryPage() {
  const buckets = useQuery({ queryKey: ['buckets'], queryFn: ({ signal }) => api.buckets(signal), staleTime: 10_000 })
  const [bucket, setBucket] = useState('default')
  const activeBucket = buckets.data?.find((item) => item.id === bucket)
  const stats = useQuery({
    queryKey: ['stats', bucket],
    queryFn: ({ signal }) => api.stats(signal, bucket),
    staleTime: 30_000,
    enabled: Boolean(activeBucket?.ready),
  })
  const firstColumn = stats.data?.column_stats[0]?.name ?? ''
  const [filters, setFilters] = useState<DraftFilter[]>([newFilter()])
  const [limit, setLimit] = useState(100)
  const [timeout, setTimeoutValue] = useState(30_000)
  const [rowsExamined, setRowsExamined] = useState(5_000_000)
  const [result, setResult] = useState<QueryResponse>()
  const [cursor, setCursor] = useState<number | null>(null)

  useEffect(() => {
    if (buckets.data?.length && !buckets.data.some((item) => item.id === bucket && item.ready)) {
      const firstReady = buckets.data.find((item) => item.ready)
      if (firstReady) setBucket(firstReady.id)
    }
  }, [buckets.data, bucket])

  useEffect(() => {
    if (firstColumn) setFilters((current) => current.map((item) => item.column ? item : { ...item, column: firstColumn }))
  }, [firstColumn])

  useEffect(() => {
    setResult(undefined)
    setCursor(null)
    setFilters([newFilter()])
  }, [bucket])

  const request = useMemo<QueryRequest>(() => ({
    bucket,
    filters: filters.map<QueryFilter>((filter) => {
      if (filter.op === 'eq') return { op: 'eq', column: filter.column, value: filter.isNull ? null : filter.value }
      if (filter.op === 'in') return { op: 'in', column: filter.column, values: filter.value.split(',').map((item) => item.trim()).filter(Boolean) }
      return { op: 'range', column: filter.column, gte: filter.value || undefined, lte: filter.upper || undefined }
    }),
    limit,
    max_rows_examined: rowsExamined,
    timeout_ms: timeout,
    after_row_id: cursor,
  }), [bucket, filters, limit, rowsExamined, timeout, cursor])

  const run = useMutation({ mutationFn: (body: QueryRequest) => api.query(body), onSuccess: setResult })
  const execute = (nextCursor: number | null) => {
    const body = { ...request, after_row_id: nextCursor }
    setCursor(nextCursor)
    run.mutate(body)
  }
  const canRun = Boolean(activeBucket?.ready) && filters.length > 0 && filters.every((filter) => filter.column && (
    filter.op === 'eq' ? filter.isNull || filter.value.length > 0
      : filter.op === 'in' ? filter.value.split(',').some((item) => item.trim())
        : Boolean(filter.value || filter.upper)
  ))

  return <div className="page stack stack--lg">
    <PageHeader eyebrow="Exact query" title="Query" description="Choose a bucket, add conditions, and page through exact results. LHR chooses exact indexes inside that bucket automatically." action={buckets.data?.length ? <label className="bucket-select"><span>Bucket</span><select value={bucket} onChange={(event) => setBucket(event.target.value)}>{buckets.data.map((item) => <option key={item.id} value={item.id} disabled={!item.ready}>{item.name}{item.ready ? '' : ' · empty'}</option>)}</select></label> : undefined} />

    <Panel title="Conditions" action={<button className="button button--primary" type="button" disabled={!canRun || run.isPending} onClick={() => execute(null)}>{run.isPending ? 'Running…' : 'Run query'}</button>}>
      <div className="stack">
        <div className="filter-list">
          {filters.map((filter) => <div className="filter-row filter-row--simple" key={filter.id}>
            <select aria-label="Column" value={filter.column} onChange={(event) => setFilters((current) => current.map((item) => item.id === filter.id ? { ...item, column: event.target.value } : item))}>
              {(stats.data?.column_stats ?? []).map((item) => <option key={item.id} value={item.name}>{item.name}</option>)}
            </select>
            <select aria-label="Operator" value={filter.op} onChange={(event) => setFilters((current) => current.map((item) => item.id === filter.id ? { ...item, op: event.target.value as DraftFilter['op'], isNull: false } : item))}>
              <option value="eq">equals</option>
              <option value="in">is one of</option>
              <option value="range">is between</option>
            </select>
            <input aria-label="Value" value={filter.value} disabled={filter.isNull} onChange={(event) => setFilters((current) => current.map((item) => item.id === filter.id ? { ...item, value: event.target.value } : item))} placeholder={filter.op === 'in' ? 'value 1, value 2' : filter.op === 'range' ? 'minimum' : 'value'} />
            {filter.op === 'range' ? <input aria-label="Maximum" value={filter.upper} onChange={(event) => setFilters((current) => current.map((item) => item.id === filter.id ? { ...item, upper: event.target.value } : item))} placeholder="maximum" /> : null}
            {filter.op === 'eq' ? <label className="check-field check-field--query"><input type="checkbox" checked={filter.isNull} onChange={(event) => setFilters((current) => current.map((item) => item.id === filter.id ? { ...item, isNull: event.target.checked } : item))} /><span>NULL</span></label> : null}
            <button className="icon-button" type="button" aria-label="Remove condition" disabled={filters.length === 1} onClick={() => setFilters((current) => current.filter((item) => item.id !== filter.id))}>×</button>
          </div>)}
        </div>

        <div className="query-toolbar">
          <button className="button button--quiet" type="button" onClick={() => setFilters((current) => [...current, newFilter(firstColumn)])}>+ Add condition</button>
          <label className="inline-field"><span>Return</span><input type="number" min={1} max={10000} value={limit} onChange={(event) => setLimit(Math.max(1, Math.min(10_000, Number(event.target.value) || 1)))} /><span>rows</span></label>
        </div>

        <details className="advanced-disclosure">
          <summary>Advanced execution limits</summary>
          <div className="query-limits">
            <label className="field"><span>Rows examined ceiling</span><input type="number" min={1} value={rowsExamined} onChange={(event) => setRowsExamined(Number(event.target.value) || 1)} /></label>
            <label className="field"><span>Timeout</span><div className="input-suffix"><input type="number" min={1} value={timeout} onChange={(event) => setTimeoutValue(Number(event.target.value) || 1)} /><span>ms</span></div></label>
          </div>
        </details>
      </div>
    </Panel>

    {run.error ? <Notice title="Query failed">{run.error.message}</Notice> : null}

    {result ? <div className="query-summary">
      <span><strong>{numberFormat.format(result.stats.hits)}</strong> matches</span>
      <span><strong>{result.returned}</strong> returned</span>
      <span><strong>{formatMicros(result.stats.elapsed_micros)}</strong></span>
      <span className="query-summary__detail">{result.stats.optimized_equality_route ? 'exact index route' : `${numberFormat.format(result.stats.rows_examined)} rows examined`}</span>
    </div> : null}

    <Panel title="Results" action={result?.next_cursor ? <button className="button button--quiet" disabled={run.isPending} onClick={() => execute(result.next_cursor)}>Next page</button> : undefined}>
      <ResultTable result={result} />
    </Panel>
  </div>
}
