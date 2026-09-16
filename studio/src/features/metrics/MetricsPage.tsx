import { useMemo } from 'react'
import { useQuery } from '@tanstack/react-query'
import { api } from '../../api/client'
import { numberFormat } from '../../components/format'
import { Notice, PageHeader, Panel } from '../../components/ui'

type MetricRow = { name: string; value: number }
function parseMetrics(text = ''): MetricRow[] {
  return text.split('\n').map((line) => line.trim()).filter((line) => line && !line.startsWith('#')).map((line) => {
    const [name, raw] = line.split(/\s+/, 2)
    return { name, value: Number(raw) }
  }).filter((row) => row.name && Number.isFinite(row.value))
}

export function MetricsPage() {
  const metrics = useQuery({ queryKey: ['metrics'], queryFn: ({ signal }) => api.metrics(signal), staleTime: 4000, refetchInterval: 5000 })
  const rows = useMemo(() => parseMetrics(metrics.data), [metrics.data])
  return <div className="page stack stack--lg">
    <PageHeader eyebrow="Process observability" title="Metrics" description="A restrained live view of LHR process, I/O, request, snapshot, and dataset counters. Polling stops when this screen is unmounted." action={<button className="button button--quiet" onClick={() => void metrics.refetch()}>Refresh</button>} />
    {metrics.error ? <Notice title="Metrics unavailable">{metrics.error.message}</Notice> : null}
    <Panel title="Prometheus counters" eyebrow="5 second active-screen refresh"><div className="metric-list">{rows.map((row) => <div className="metric-line" key={row.name}><span className="mono">{row.name}</span><strong className="mono">{numberFormat.format(row.value)}</strong></div>)}{!metrics.isPending && !rows.length ? <div className="empty-state">No process counters were returned.</div> : null}</div></Panel>
  </div>
}
