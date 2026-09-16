import { useQuery } from '@tanstack/react-query'
import { api } from '../../api/client'
import { numberFormat } from '../../components/format'
import { Notice, PageHeader, Panel, StatusMark } from '../../components/ui'

export function GenerationsPage() {
  const generations = useQuery({ queryKey: ['generations'], queryFn: ({ signal }) => api.generations(signal), staleTime: 20_000 })
  return <div className="page stack stack--lg">
    <PageHeader eyebrow="Immutable publication" title="Generations" description="Inspect the catalog timeline. Readers remain pinned to immutable snapshots while new generations are published atomically." action={<button className="button button--quiet" onClick={() => void generations.refetch()}>Refresh</button>} />
    {generations.error ? <Notice title="Catalog unavailable">{generations.error.message}</Notice> : null}
    <Panel title="Generation catalog" eyebrow="CURRENT and retained history"><div className="table-wrap"><table className="data-table"><thead><tr><th>ID</th><th>State</th><th>Integrity</th><th>Path</th></tr></thead><tbody>{(generations.data ?? []).slice().reverse().map((item) => <tr key={item.id}><td className="mono data-table__primary">{String(item.id).padStart(20, '0')}</td><td><span className="cluster cluster--tight"><StatusMark active={item.current} />{item.current ? 'CURRENT' : 'retained'}</span></td><td>{item.sealed ? 'sealed' : 'unsealed'}</td><td className="mono table-path" title={item.path}>{item.path}</td></tr>)}{!generations.isPending && !generations.data?.length ? <tr><td colSpan={4} className="data-table__empty">No published generations found.</td></tr> : null}</tbody></table></div></Panel>
    <div className="notice"><strong>Rollback stays local.</strong><span>Catalog rollback, backup, and restore remain explicit CLI operations so an HTTP credential cannot directly turn into arbitrary filesystem access.</span></div>
  </div>
}
