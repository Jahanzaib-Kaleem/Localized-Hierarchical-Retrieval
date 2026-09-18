import { useEffect, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { api } from '../../api/client'
import { numberFormat } from '../../components/format'
import { Notice, PageHeader, Panel, StatusMark } from '../../components/ui'

export function GenerationsPage() {
  const buckets = useQuery({ queryKey: ['buckets'], queryFn: ({ signal }) => api.buckets(signal), staleTime: 15_000 })
  const [bucket, setBucket] = useState('default')
  const activeBucket = buckets.data?.find((item) => item.id === bucket)
  useEffect(() => {
    if (buckets.data?.length && !buckets.data.some((item) => item.id === bucket && item.ready)) {
      const firstReady = buckets.data.find((item) => item.ready)
      if (firstReady) setBucket(firstReady.id)
    }
  }, [buckets.data, bucket])
  const generations = useQuery({ queryKey: ['generations', bucket], queryFn: ({ signal }) => api.generations(signal, bucket), staleTime: 20_000, enabled: Boolean(activeBucket?.ready) })
  return <div className="page stack stack--lg">
    <PageHeader eyebrow="Immutable publication" title="Generations" description="Inspect one bucket's catalog timeline. Readers remain pinned to immutable snapshots while new generations are published atomically." action={<div className="button-group"><label className="bucket-select"><span>Bucket</span><select value={bucket} onChange={(event) => setBucket(event.target.value)}>{(buckets.data ?? []).map((item) => <option key={item.id} value={item.id} disabled={!item.ready}>{item.name}{item.ready ? '' : ' · empty'}</option>)}</select></label><button className="button button--quiet" disabled={!activeBucket?.ready} onClick={() => void generations.refetch()}>Refresh</button></div>} />
    {generations.error ? <Notice title="Catalog unavailable">{generations.error.message}</Notice> : null}
    <Panel title="Generation catalog" eyebrow="CURRENT and retained history"><div className="table-wrap"><table className="data-table"><thead><tr><th>ID</th><th>State</th><th>Integrity</th><th>Path</th></tr></thead><tbody>{(generations.data ?? []).slice().reverse().map((item) => <tr key={item.id}><td className="mono data-table__primary">{String(item.id).padStart(20, '0')}</td><td><span className="cluster cluster--tight"><StatusMark active={item.current} />{item.current ? 'CURRENT' : 'retained'}</span></td><td>{item.sealed ? 'sealed' : 'unsealed'}</td><td className="mono table-path" title={item.path}>{item.path}</td></tr>)}{!generations.isPending && !generations.data?.length ? <tr><td colSpan={4} className="data-table__empty">No published generations found.</td></tr> : null}</tbody></table></div></Panel>
    <div className="notice"><strong>Rollback stays local.</strong><span>Catalog rollback, backup, and restore remain explicit CLI operations so an HTTP credential cannot directly turn into arbitrary filesystem access.</span></div>
  </div>
}
