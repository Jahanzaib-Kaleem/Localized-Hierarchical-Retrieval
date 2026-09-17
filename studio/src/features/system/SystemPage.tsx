import { Link } from '@tanstack/react-router'
import { useQuery } from '@tanstack/react-query'
import { api } from '../../api/client'
import { ArrowIcon, GenerationsIcon, IndexIcon, MetricsIcon, OperationsIcon, WorkloadIcon } from '../../components/icons'
import { formatBytes, numberFormat } from '../../components/format'
import { Metric, PageHeader, StatusMark } from '../../components/ui'

const tools = [
  { to: '/indexes' as const, title: 'Indexes', description: 'Inspect exact indexes and manage multi-column accelerators.', icon: IndexIcon },
  { to: '/workload' as const, title: 'Workload', description: 'See query latency, shapes, index use, and recommendations.', icon: WorkloadIcon },
  { to: '/generations' as const, title: 'Generations', description: 'Inspect immutable generations and the currently published snapshot.', icon: GenerationsIcon },
  { to: '/operations' as const, title: 'Maintenance', description: 'Compaction, vacuum, and recovery actions.', icon: OperationsIcon },
  { to: '/metrics' as const, title: 'Metrics', description: 'Raw process and service counters for diagnostics.', icon: MetricsIcon },
]

export function SystemPage() {
  const health = useQuery({ queryKey: ['health'], queryFn: ({ signal }) => api.health(signal), staleTime: 10_000, refetchInterval: 30_000 })
  const ready = useQuery({ queryKey: ['ready'], queryFn: ({ signal }) => api.ready(signal), staleTime: 10_000, refetchInterval: 30_000 })
  const stats = useQuery({ queryKey: ['stats'], queryFn: ({ signal }) => api.stats(signal), staleTime: 30_000, retry: false })
  const isHealthy = health.data?.status === 'ok'
  const isReady = ready.data?.status === 'ready'

  return <div className="page stack stack--lg">
    <PageHeader eyebrow="Advanced" title="System" description="Operational tooling for maintaining and diagnosing LHR. Most day-to-day work should happen in Data, Query, or API." />

    <div className="system-health">
      <div><StatusMark active={isHealthy} /><span>Service</span><strong>{isHealthy ? 'Healthy' : 'Unavailable'}</strong></div>
      <div><StatusMark active={isReady} /><span>Dataset</span><strong>{isReady ? 'Ready' : 'Not ready'}</strong></div>
      <div><span>Rows</span><strong>{stats.data ? numberFormat.format(stats.data.rows) : '—'}</strong></div>
      <div><span>Storage</span><strong>{stats.data ? formatBytes(stats.data.total_bytes) : '—'}</strong></div>
    </div>

    <div className="tool-grid">
      {tools.map(({ to, title, description, icon: Icon }) => <Link className="tool-card" key={to} to={to}>
        <div className="tool-card__icon"><Icon /></div>
        <div className="tool-card__copy"><strong>{title}</strong><span>{description}</span></div>
        <ArrowIcon className="tool-card__arrow" />
      </Link>)}
    </div>
  </div>
}
