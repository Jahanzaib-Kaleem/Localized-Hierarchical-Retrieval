import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api } from '../../api/client'
import type { MutationOperation, QueryResponse } from '../../api/types'
import { formatBytes, numberFormat } from '../../components/format'
import { ResultTable } from '../../components/ResultTable'
import { ActionResult, Notice, PageHeader, Panel } from '../../components/ui'

const mutationExample = `[
  {
    "op": "update",
    "row_id": 42,
    "values": { "company": "Example" }
  }
]`

export function DataPage() {
  const queryClient = useQueryClient()
  const stats = useQuery({ queryKey: ['stats'], queryFn: ({ signal }) => api.stats(signal), staleTime: 30_000 })
  const [column, setColumn] = useState('')
  const [value, setValue] = useState('')
  const [limit, setLimit] = useState(50)
  const [result, setResult] = useState<QueryResponse>()
  const [mutationText, setMutationText] = useState(mutationExample)
  const [mutationError, setMutationError] = useState('')

  useEffect(() => { if (!column && stats.data?.column_stats[0]) setColumn(stats.data.column_stats[0].name) }, [column, stats.data])

  const lookup = useMutation({ mutationFn: () => api.query({ filters: [{ op: 'eq', column, value }], limit, max_rows_examined: 1_000_000, timeout_ms: 5000 }), onSuccess: setResult })
  const mutate = useMutation({
    mutationFn: async () => {
      setMutationError('')
      const parsed: unknown = JSON.parse(mutationText)
      if (!Array.isArray(parsed) || parsed.length === 0) throw new Error('Mutation payload must be a non-empty JSON array.')
      return api.mutate(parsed as MutationOperation[])
    },
    onSuccess: async () => { await queryClient.invalidateQueries({ queryKey: ['stats'] }); await queryClient.invalidateQueries({ queryKey: ['generations'] }) },
    onError: (error) => setMutationError(error instanceof Error ? error.message : String(error)),
  })

  return <div className="page stack stack--lg">
    <PageHeader eyebrow="Logical data" title="Data" description="Inspect exact bounded result sets and submit explicit transactional row mutations without materializing the dataset in the browser." />
    {stats.error ? <Notice title="Schema unavailable">{stats.error.message}</Notice> : null}
    <div className="content-grid content-grid--3-2">
      <Panel title="Exact row lookup" eyebrow="Bounded read">
        <form className="form-grid" onSubmit={(event) => { event.preventDefault(); if (column) lookup.mutate() }}>
          <label className="field"><span>Column</span><select value={column} onChange={(event) => setColumn(event.target.value)}>{(stats.data?.column_stats ?? []).map((item) => <option key={item.id} value={item.name}>{item.name}</option>)}</select></label>
          <label className="field field--grow"><span>Value</span><input value={value} onChange={(event) => setValue(event.target.value)} placeholder="Exact canonical value" /></label>
          <label className="field field--compact"><span>Rows</span><input type="number" min={1} max={500} value={limit} onChange={(event) => setLimit(Math.max(1, Math.min(500, Number(event.target.value) || 1)))} /></label>
          <button className="button" disabled={!column || lookup.isPending} type="submit">{lookup.isPending ? 'Running…' : 'Run lookup'}</button>
        </form>
        {lookup.error ? <Notice title="Query failed">{lookup.error.message}</Notice> : null}
      </Panel>
      <Panel title="Dataset contract" eyebrow="Current schema">
        <dl className="definition-list"><div><dt>Rows</dt><dd>{stats.data ? numberFormat.format(stats.data.rows) : '—'}</dd></div><div><dt>Columns</dt><dd>{stats.data ? numberFormat.format(stats.data.columns) : '—'}</dd></div><div><dt>Total bytes</dt><dd>{stats.data ? formatBytes(stats.data.total_bytes) : '—'}</dd></div><div><dt>Read policy</dt><dd>bounded + cursor-safe</dd></div></dl>
      </Panel>
    </div>
    <Panel title="Rows" eyebrow="Query result"><ResultTable result={result} /></Panel>
    <Panel title="Mutation transaction" eyebrow="Write API">
      <div className="stack"><p className="support-copy">Submit one JSON array of insert, update, and/or delete operations. The server validates the complete transaction and publishes an immutable delta generation. Studio never retries writes automatically.</p><textarea className="code-editor" spellCheck={false} value={mutationText} onChange={(event) => setMutationText(event.target.value)} />
      <div className="cluster cluster--spread"><span className="field-hint">Large bulk ingest remains a local CLI operation by design.</span><button className="button" type="button" disabled={mutate.isPending} onClick={() => { if (window.confirm('Apply this mutation transaction?')) mutate.mutate() }}>{mutate.isPending ? 'Applying…' : 'Apply transaction'}</button></div>
      {mutationError ? <Notice title="Mutation rejected">{mutationError}</Notice> : null}{mutate.data ? <ActionResult value={mutate.data} /> : null}</div>
    </Panel>
  </div>
}
