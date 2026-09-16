import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api } from '../../api/client'
import { formatBytes, numberFormat } from '../../components/format'
import { ActionResult, Notice, PageHeader, Panel } from '../../components/ui'

export function IndexesPage() {
  const queryClient = useQueryClient()
  const stats = useQuery({ queryKey: ['stats'], queryFn: ({ signal }) => api.stats(signal), staleTime: 30_000 })
  const [columns, setColumns] = useState('')
  const change = useMutation({
    mutationFn: ({ action, names }: { action: 'add' | 'drop' | 'rebuild'; names: string[] }) => api.indexChange(action, names),
    onSuccess: async () => { await queryClient.invalidateQueries({ queryKey: ['stats'] }); await queryClient.invalidateQueries({ queryKey: ['generations'] }); await queryClient.invalidateQueries({ queryKey: ['workload'] }) },
  })
  const names = columns.split(',').map((value) => value.trim()).filter(Boolean)
  const act = (action: 'add' | 'drop' | 'rebuild') => { if (names.length >= 2 && window.confirm(`${action} accelerator for ${names.join(', ')}?`)) change.mutate({ action, names }) }
  return <div className="page stack stack--lg">
    <PageHeader eyebrow="Physical acceleration" title="Indexes" description="Inspect exact structures and administer multi-column accelerators. Index changes affect performance, never logical correctness." />
    <Panel title="Accelerator administration" eyebrow="Generation transaction">
      <div className="form-grid"><label className="field field--grow"><span>Columns</span><input value={columns} onChange={(event) => setColumns(event.target.value)} placeholder="country, industry" /><small>Comma-separated; accelerator indexes require at least two unique columns.</small></label><div className="button-group"><button className="button" disabled={names.length < 2 || change.isPending} onClick={() => act('add')}>Add</button><button className="button button--quiet" disabled={names.length < 2 || change.isPending} onClick={() => act('rebuild')}>Rebuild</button><button className="button button--quiet" disabled={names.length < 2 || change.isPending} onClick={() => act('drop')}>Drop</button></div></div>
      {change.error ? <Notice title="Index operation failed">{change.error.message}</Notice> : null}{change.data ? <ActionResult value={change.data} /> : null}
    </Panel>
    <Panel title="Installed structures" eyebrow="Current generation">
      <div className="table-wrap"><table className="data-table"><thead><tr><th>Columns</th><th>Kind</th><th>Entries</th><th>Keyspace</th><th>Bytes</th><th>Exact rows</th><th>File</th></tr></thead><tbody>{(stats.data?.indexes ?? []).map((index) => <tr key={index.file}><td className="data-table__primary">{index.column_names.join(' + ')}</td><td><span className="code-chip">{index.kind}</span></td><td className="mono">{numberFormat.format(index.entries)}</td><td className="mono">{numberFormat.format(index.keyspace)}</td><td className="mono">{formatBytes(index.bytes)}</td><td>{index.exact_rows ? 'yes' : 'routing'}</td><td className="mono table-path">{index.file}</td></tr>)}{!stats.isPending && !stats.data?.indexes.length ? <tr><td colSpan={7} className="data-table__empty">No indexes reported.</td></tr> : null}</tbody></table></div>
    </Panel>
  </div>
}
