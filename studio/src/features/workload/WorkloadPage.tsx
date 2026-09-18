import { useEffect, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { api } from '../../api/client'
import { formatMicros, numberFormat } from '../../components/format'
import { Metric, Notice, PageHeader, Panel } from '../../components/ui'

export function WorkloadPage() {
  const buckets = useQuery({ queryKey: ['buckets'], queryFn: ({ signal }) => api.buckets(signal), staleTime: 15_000 })
  const [bucket, setBucket] = useState('default')
  const activeBucket = buckets.data?.find((item) => item.id === bucket)
  useEffect(() => {
    if (buckets.data?.length && !buckets.data.some((item) => item.id === bucket && item.ready)) {
      const firstReady = buckets.data.find((item) => item.ready)
      if (firstReady) setBucket(firstReady.id)
    }
  }, [buckets.data, bucket])
  const workload = useQuery({ queryKey: ['workload', bucket], queryFn: ({ signal }) => api.workload(signal, bucket), staleTime: 30_000, enabled: Boolean(activeBucket?.ready) })
  const data = workload.data
  return <div className="page stack stack--lg">
    <PageHeader eyebrow="Observed execution" title="Workload" description="Persistent query-shape telemetry and index recommendations for one bucket, derived from real database work." action={<div className="button-group"><label className="bucket-select"><span>Bucket</span><select value={bucket} onChange={(event) => setBucket(event.target.value)}>{(buckets.data ?? []).map((item) => <option key={item.id} value={item.id} disabled={!item.ready}>{item.name}{item.ready ? '' : ' · empty'}</option>)}</select></label><button className="button button--quiet" disabled={!activeBucket?.ready} onClick={() => void workload.refetch()}>Refresh</button></div>} />
    {workload.error ? <Notice title="Workload unavailable">{workload.error.message}</Notice> : null}
    <div className="metric-grid"><Metric label="Queries" value={data ? numberFormat.format(data.query_count) : '—'} detail="persistent telemetry" /><Metric label="P50" value={data ? formatMicros(data.p50_micros) : '—'} detail="all queries" /><Metric label="P95" value={data ? formatMicros(data.p95_micros) : '—'} detail="all queries" /><Metric label="P99" value={data ? formatMicros(data.p99_micros) : '—'} detail="all queries" /></div>
    <Panel title="Query shapes" eyebrow="Latency and scan work"><div className="table-wrap"><table className="data-table"><thead><tr><th>Columns</th><th>Operators</th><th>Queries</th><th>P50</th><th>P95</th><th>P99</th><th>Avg rows</th><th>Max rows</th></tr></thead><tbody>{(data?.shapes ?? []).map((shape, index) => <tr key={`${shape.columns.join('|')}-${index}`}><td className="data-table__primary">{shape.columns.join(', ')}</td><td>{shape.operators.map((op) => <span className="code-chip" key={op}>{op}</span>)}</td><td className="mono">{numberFormat.format(shape.queries)}</td><td className="mono">{formatMicros(shape.p50_micros)}</td><td className="mono">{formatMicros(shape.p95_micros)}</td><td className="mono">{formatMicros(shape.p99_micros)}</td><td className="mono">{numberFormat.format(Math.round(shape.avg_rows_examined))}</td><td className="mono">{numberFormat.format(shape.max_rows_examined)}</td></tr>)}{!workload.isPending && !data?.shapes.length ? <tr><td colSpan={8} className="data-table__empty">No query telemetry has been recorded yet.</td></tr> : null}</tbody></table></div></Panel>
    <div className="content-grid content-grid--3-2"><Panel title="Recommendations" eyebrow="Workload-derived accelerators"><div className="stack stack--sm">{(data?.recommendations ?? []).map((item) => <div className="recommendation" key={item.columns.join('|')}><div><strong>{item.columns.join(' + ')}</strong><span>{item.reason}</span></div><div className="mono">{numberFormat.format(item.observed_queries)} q · {numberFormat.format(Math.round(item.avg_rows_examined))} avg rows</div></div>)}{!data?.recommendations.length ? <div className="empty-state">No unindexed multi-column equality pattern is currently recommended.</div> : null}</div></Panel><Panel title="Index use" eyebrow="Planner-selected structures"><div className="stack stack--sm">{(data?.index_use ?? []).map((item) => <div className="storage-row" key={item.index}><span className="mono text-truncate" title={item.index}>{item.index}</span><strong>{numberFormat.format(item.queries)}</strong></div>)}{!data?.index_use.length ? <div className="empty-state">No planner index-use events yet.</div> : null}</div></Panel></div>
  </div>
}
