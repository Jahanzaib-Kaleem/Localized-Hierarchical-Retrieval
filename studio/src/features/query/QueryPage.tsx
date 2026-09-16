import { useEffect, useMemo, useState } from 'react'
import { useMutation, useQuery } from '@tanstack/react-query'
import { api } from '../../api/client'
import type { QueryFilter, QueryRequest, QueryResponse } from '../../api/types'
import { formatMicros, numberFormat } from '../../components/format'
import { ResultTable } from '../../components/ResultTable'
import { Metric, Notice, PageHeader, Panel } from '../../components/ui'

type DraftFilter = { id: number; column: string; op: 'eq' | 'in' | 'range'; value: string; upper: string; isNull: boolean }
let filterId = 1

export function QueryPage() {
  const stats = useQuery({ queryKey: ['stats'], queryFn: ({ signal }) => api.stats(signal), staleTime: 30_000 })
  const firstColumn = stats.data?.column_stats[0]?.name ?? ''
  const [filters, setFilters] = useState<DraftFilter[]>([{ id: filterId++, column: '', op: 'eq', value: '', upper: '', isNull: false }])
  const [limit, setLimit] = useState(100)
  const [timeout, setTimeoutValue] = useState(5000)
  const [rowsExamined, setRowsExamined] = useState(1_000_000)
  const [result, setResult] = useState<QueryResponse>()
  const [cursor, setCursor] = useState<number | null>(null)
  useEffect(() => { if (firstColumn) setFilters((current) => current.map((item) => item.column ? item : { ...item, column: firstColumn })) }, [firstColumn])

  const request = useMemo<QueryRequest>(() => ({
    filters: filters.map<QueryFilter>((filter) => {
      if (filter.op === 'eq') return { op: 'eq', column: filter.column, value: filter.isNull ? null : filter.value }
      if (filter.op === 'in') return { op: 'in', column: filter.column, values: filter.value.split(',').map((item) => item.trim()).filter(Boolean) }
      return { op: 'range', column: filter.column, gte: filter.value || undefined, lte: filter.upper || undefined }
    }),
    limit, max_rows_examined: rowsExamined, timeout_ms: timeout, after_row_id: cursor,
  }), [filters, limit, rowsExamined, timeout, cursor])

  const run = useMutation({ mutationFn: (body: QueryRequest) => api.query(body), onSuccess: setResult })
  const execute = (nextCursor: number | null) => { const body = { ...request, after_row_id: nextCursor }; setCursor(nextCursor); run.mutate(body) }
  const canRun = filters.length > 0 && filters.every((filter) => filter.column && (filter.op === 'eq' ? filter.isNull || filter.value.length > 0 : filter.op === 'in' ? filter.value.split(',').some((item) => item.trim()) : Boolean(filter.value || filter.upper)))

  return <div className="page stack stack--lg">
    <PageHeader eyebrow="Exact retrieval" title="Query" description="Build typed exact predicates, keep execution ceilings explicit, and inspect the amount of database work behind every page." />
    <Panel title="Predicate builder" eyebrow="Query request">
      <div className="stack">
        <div className="filter-list">{filters.map((filter, index) => <div className="filter-row" key={filter.id}>
          <span className="filter-row__index mono">{String(index + 1).padStart(2, '0')}</span>
          <select value={filter.column} onChange={(event) => setFilters((current) => current.map((item) => item.id === filter.id ? { ...item, column: event.target.value } : item))}>{(stats.data?.column_stats ?? []).map((item) => <option key={item.id} value={item.name}>{item.name}</option>)}</select>
          <select value={filter.op} onChange={(event) => setFilters((current) => current.map((item) => item.id === filter.id ? { ...item, op: event.target.value as DraftFilter['op'], isNull: false } : item))}><option value="eq">equals</option><option value="in">in set</option><option value="range">range</option></select>
          <input value={filter.value} disabled={filter.isNull} onChange={(event) => setFilters((current) => current.map((item) => item.id === filter.id ? { ...item, value: event.target.value } : item))} placeholder={filter.op === 'in' ? 'a, b, c' : filter.op === 'range' ? 'lower bound' : 'value'} />
          {filter.op === 'range' ? <input value={filter.upper} onChange={(event) => setFilters((current) => current.map((item) => item.id === filter.id ? { ...item, upper: event.target.value } : item))} placeholder="upper bound" /> : null}
          {filter.op === 'eq' ? <label className="check-field"><input type="checkbox" checked={filter.isNull} onChange={(event) => setFilters((current) => current.map((item) => item.id === filter.id ? { ...item, isNull: event.target.checked } : item))} /><span>NULL</span></label> : null}
          <button className="icon-button" type="button" aria-label="Remove filter" disabled={filters.length === 1} onClick={() => setFilters((current) => current.filter((item) => item.id !== filter.id))}>×</button>
        </div>)}</div>
        <div className="cluster"><button className="button button--quiet" type="button" onClick={() => setFilters((current) => [...current, { id: filterId++, column: firstColumn, op: 'eq', value: '', upper: '', isNull: false }])}>Add predicate</button></div>
        <div className="query-limits"><label className="field"><span>Return limit</span><input type="number" min={1} max={10000} value={limit} onChange={(event) => setLimit(Number(event.target.value) || 1)} /></label><label className="field"><span>Rows examined ceiling</span><input type="number" min={1} value={rowsExamined} onChange={(event) => setRowsExamined(Number(event.target.value) || 1)} /></label><label className="field"><span>Timeout ms</span><input type="number" min={1} value={timeout} onChange={(event) => setTimeoutValue(Number(event.target.value) || 1)} /></label></div>
        <div className="cluster cluster--spread"><span className="field-hint">Pure equality retains the optimized exact-index route.</span><button className="button" type="button" disabled={!canRun || run.isPending} onClick={() => execute(null)}>{run.isPending ? 'Executing…' : 'Execute query'}</button></div>
      </div>
    </Panel>
    {run.error ? <Notice title="Query failed">{run.error.message}</Notice> : null}
    {result ? <div className="metric-grid"><Metric label="Hits" value={numberFormat.format(result.stats.hits)} detail={`${result.returned} returned`} /><Metric label="Elapsed" value={formatMicros(result.stats.elapsed_micros)} detail={result.stats.optimized_equality_route ? 'optimized equality route' : 'exact fallback'} /><Metric label="Rows examined" value={numberFormat.format(result.stats.rows_examined)} detail="bounded by request" /><Metric label="Pages touched" value={numberFormat.format(result.stats.pages_touched)} detail={`${numberFormat.format(result.stats.hierarchy_lookups)} hierarchy lookups`} /></div> : null}
    <Panel title="Results" eyebrow="Stable logical row order" action={result?.next_cursor ? <button className="button button--quiet" disabled={run.isPending} onClick={() => execute(result.next_cursor)}>Next page</button> : undefined}><ResultTable result={result} /></Panel>
  </div>
}
