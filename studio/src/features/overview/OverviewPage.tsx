import { Link } from '@tanstack/react-router'
import { useQuery } from '@tanstack/react-query'
import { api } from '../../api/client'
import { formatBytes, numberFormat } from '../../components/format'
import { ApiIcon, ArrowIcon, DataIcon, QueryIcon, UploadIcon } from '../../components/icons'
import { Metric, Notice, PageHeader, Panel } from '../../components/ui'

export function OverviewPage() {
  const ready = useQuery({ queryKey: ['ready'], queryFn: ({ signal }) => api.ready(signal), staleTime: 10_000, refetchInterval: 30_000 })
  const stats = useQuery({ queryKey: ['stats'], queryFn: ({ signal }) => api.stats(signal), staleTime: 30_000, retry: false })
  const isReady = ready.data?.status === 'ready'

  return <div className="page stack stack--lg">
    <PageHeader eyebrow="Workspace" title="Your LHR database" description={isReady ? 'Import data, run exact queries, or connect your application.' : 'Get a dataset into LHR, then query it from Studio or your application.'} />

    {!isReady ? <section className="onboarding-card">
      <div className="onboarding-card__icon"><UploadIcon /></div>
      <div className="onboarding-card__copy">
        <span className="eyebrow">Start here</span>
        <h2>Add your first dataset</h2>
        <p>Bring in a CSV, review the detected columns, and create an exact indexed dataset.</p>
      </div>
      <Link className="button button--primary" to="/data">Import data <ArrowIcon className="button__icon" /></Link>
    </section> : null}

    {ready.error ? <Notice title="LHR is not responding">Check the service connection in Settings.</Notice> : null}

    <div className="quick-action-grid">
      <Link className="quick-action" to="/data"><div className="quick-action__icon"><DataIcon /></div><div><strong>Data</strong><span>Import CSVs and inspect your schema.</span></div><ArrowIcon /></Link>
      <Link className="quick-action" to="/query"><div className="quick-action__icon"><QueryIcon /></div><div><strong>Query</strong><span>Build exact filters and inspect results.</span></div><ArrowIcon /></Link>
      <Link className="quick-action" to="/api"><div className="quick-action__icon"><ApiIcon /></div><div><strong>API</strong><span>Copy a working request into your app.</span></div><ArrowIcon /></Link>
    </div>

    {stats.data ? <>
      <div className="metric-grid metric-grid--home">
        <Metric label="Rows" value={numberFormat.format(stats.data.rows)} />
        <Metric label="Columns" value={numberFormat.format(stats.data.columns)} />
        <Metric label="Storage" value={formatBytes(stats.data.total_bytes)} />
        <Metric label="Indexes" value={numberFormat.format(stats.data.indexes.length)} />
      </div>

      <Panel title="Schema" eyebrow="Current dataset" action={<Link className="text-link" to="/data">Open data <ArrowIcon /></Link>}>
        <div className="schema-list">
          {stats.data.column_stats.slice(0, 8).map((column) => <div className="schema-row" key={column.id}>
            <div><strong>{column.name}</strong><span>{column.nullable ? 'nullable' : 'required'}</span></div>
            <span className="code-chip">{column.logical_type}</span>
            <span className="schema-row__meta">{numberFormat.format(column.cardinality)} values</span>
          </div>)}
          {stats.data.column_stats.length > 8 ? <Link className="schema-more" to="/data">+ {stats.data.column_stats.length - 8} more columns</Link> : null}
        </div>
      </Panel>
    </> : null}
  </div>
}
