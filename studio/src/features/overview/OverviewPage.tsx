import { useQuery } from '@tanstack/react-query'
import { api } from '../../api/client'
import { Metric, Panel, StatusMark } from '../../components/ui'
import { RefreshIcon } from '../../components/icons'

const number = new Intl.NumberFormat('en-US')

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB']
  const index = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1)
  const value = bytes / 1024 ** index
  return `${value >= 100 || index === 0 ? value.toFixed(0) : value.toFixed(1)} ${units[index]}`
}

export function OverviewPage() {
  const health = useQuery({ queryKey: ['health'], queryFn: api.health, staleTime: 10_000, refetchInterval: 30_000 })
  const ready = useQuery({ queryKey: ['ready'], queryFn: api.ready, staleTime: 10_000, refetchInterval: 30_000 })
  const stats = useQuery({ queryKey: ['stats'], queryFn: api.stats, staleTime: 30_000 })

  const isHealthy = health.data?.status === 'ok'
  const isReady = ready.data?.status === 'ready'

  return (
    <div className="page stack stack--lg">
      <header className="page-header">
        <div className="stack stack--xs">
          <span className="eyebrow">Database control plane</span>
          <h1 className="page-title">Overview</h1>
          <p className="page-description">A bounded view of the current LHR dataset, storage topology, and service state.</p>
        </div>
        <button className="button button--quiet" type="button" onClick={() => void Promise.all([health.refetch(), ready.refetch(), stats.refetch()])}>
          <RefreshIcon className="button__icon" />
          Refresh
        </button>
      </header>

      <div className="status-strip" aria-label="Service state">
        <div className="status-strip__item"><StatusMark active={isHealthy} /><span>Process</span><strong>{health.isPending ? 'Checking' : isHealthy ? 'Healthy' : 'Unavailable'}</strong></div>
        <div className="status-strip__item"><StatusMark active={isReady} /><span>Dataset</span><strong>{ready.isPending ? 'Checking' : isReady ? 'Ready' : 'Not ready'}</strong></div>
        <div className="status-strip__item status-strip__item--end"><span>Transport</span><strong>Same origin</strong></div>
      </div>

      {stats.error ? <div className="notice"><strong>Statistics unavailable.</strong><span>{stats.error.message}</span></div> : null}

      <div className="metric-grid">
        <Metric label="Logical rows" value={stats.data ? number.format(stats.data.rows) : '—'} detail="stable row IDs" />
        <Metric label="Columns" value={stats.data ? number.format(stats.data.columns) : '—'} detail={stats.data ? `${stats.data.indexes.length} exact / routing indexes` : '—'} />
        <Metric label="Dataset bytes" value={stats.data ? formatBytes(stats.data.total_bytes) : '—'} detail="canonical + routing" />
        <Metric label="Pages" value={stats.data ? number.format(stats.data.pages) : '—'} detail="canonical page topology" />
      </div>

      <div className="content-grid content-grid--3-2">
        <Panel title="Storage composition" eyebrow="Physical footprint">
          <div className="stack">
            <div className="storage-row"><span>Canonical</span><strong>{stats.data ? formatBytes(stats.data.canonical_bytes) : '—'}</strong></div>
            <div className="storage-track"><span style={{ width: stats.data?.total_bytes ? `${Math.max(2, (stats.data.canonical_bytes / stats.data.total_bytes) * 100)}%` : '0%' }} /></div>
            <div className="storage-row"><span>Routing + indexes</span><strong>{stats.data ? formatBytes(stats.data.routing_bytes) : '—'}</strong></div>
            <div className="storage-track"><span style={{ width: stats.data?.total_bytes ? `${Math.max(2, (stats.data.routing_bytes / stats.data.total_bytes) * 100)}%` : '0%' }} /></div>
            <div className="divider" />
            <div className="storage-row storage-row--total"><span>Total</span><strong>{stats.data ? formatBytes(stats.data.total_bytes) : '—'}</strong></div>
          </div>
        </Panel>

        <Panel title="Service posture" eyebrow="Runtime contract">
          <dl className="definition-list">
            <div><dt>Process</dt><dd>{isHealthy ? 'responding' : 'unavailable'}</dd></div>
            <div><dt>Dataset</dt><dd>{isReady ? 'queryable' : 'not ready'}</dd></div>
            <div><dt>Frontend</dt><dd>static client</dd></div>
            <div><dt>API path</dt><dd>/v1</dd></div>
            <div><dt>Studio policy</dt><dd>bounded requests</dd></div>
          </dl>
        </Panel>
      </div>

      <Panel title="Columns" eyebrow="Schema and dictionaries">
        <div className="table-wrap">
          <table className="data-table">
            <thead><tr><th>Column</th><th>Type</th><th>Cardinality</th><th>Dictionary</th><th>Nullable</th></tr></thead>
            <tbody>
              {(stats.data?.column_stats ?? []).map((column) => (
                <tr key={column.id}>
                  <td className="data-table__primary">{column.name}</td>
                  <td><span className="code-chip">{column.logical_type}</span></td>
                  <td className="mono">{number.format(column.cardinality)}</td>
                  <td className="mono">{formatBytes(column.dictionary_bytes)}</td>
                  <td>{column.nullable ? 'yes' : 'no'}</td>
                </tr>
              ))}
              {!stats.isPending && !stats.data?.column_stats.length ? <tr><td colSpan={5} className="data-table__empty">No columns reported.</td></tr> : null}
            </tbody>
          </table>
        </div>
      </Panel>
    </div>
  )
}
