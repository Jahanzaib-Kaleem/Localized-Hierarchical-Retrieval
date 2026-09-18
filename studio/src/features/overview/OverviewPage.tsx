import { Link } from '@tanstack/react-router'
import { useQuery } from '@tanstack/react-query'
import { api } from '../../api/client'
import { formatBytes, numberFormat } from '../../components/format'
import { ApiIcon, ArrowIcon, DataIcon, QueryIcon, UploadIcon } from '../../components/icons'
import { Metric, Notice, PageHeader, Panel, StatusMark } from '../../components/ui'

export function OverviewPage() {
  const ready = useQuery({ queryKey: ['ready'], queryFn: ({ signal }) => api.ready(signal), staleTime: 10_000, refetchInterval: 30_000 })
  const buckets = useQuery({ queryKey: ['buckets'], queryFn: ({ signal }) => api.buckets(signal), staleTime: 15_000, retry: false })
  const isReady = ready.data?.status === 'ready'
  const readyBuckets = (buckets.data ?? []).filter((bucket) => bucket.ready)
  const totalRows = readyBuckets.reduce((sum, bucket) => sum + bucket.rows, 0)
  const totalBytes = readyBuckets.reduce((sum, bucket) => sum + bucket.total_bytes, 0)

  return <div className="page stack stack--lg">
    <PageHeader eyebrow="Workspace" title="Your LHR database" description={isReady ? 'Browse buckets, work with exact data, or connect your application.' : 'Create a bucket and import data, then query it from Studio or your application.'} />

    {!isReady ? <section className="onboarding-card">
      <div className="onboarding-card__icon"><UploadIcon /></div>
      <div className="onboarding-card__copy">
        <span className="eyebrow">Start here</span>
        <h2>Add your first dataset</h2>
        <p>Create a bucket, bring in a CSV, review the detected columns, and publish an exact indexed dataset.</p>
      </div>
      <Link className="button button--primary" to="/data">Open Data <ArrowIcon className="button__icon" /></Link>
    </section> : null}

    {ready.error ? <Notice title="LHR is not responding">Check the service connection in Settings.</Notice> : null}

    <div className="quick-action-grid">
      <Link className="quick-action" to="/data"><div className="quick-action__icon"><DataIcon /></div><div><strong>Data</strong><span>Browse rows, buckets, imports, and transfers.</span></div><ArrowIcon /></Link>
      <Link className="quick-action" to="/query"><div className="quick-action__icon"><QueryIcon /></div><div><strong>Query</strong><span>Run exact filters inside any ready bucket.</span></div><ArrowIcon /></Link>
      <Link className="quick-action" to="/api"><div className="quick-action__icon"><ApiIcon /></div><div><strong>API</strong><span>Copy a bucket-aware request into your app.</span></div><ArrowIcon /></Link>
    </div>

    <div className="metric-grid metric-grid--home">
      <Metric label="Buckets" value={numberFormat.format(buckets.data?.length ?? 0)} />
      <Metric label="Ready" value={numberFormat.format(readyBuckets.length)} />
      <Metric label="Rows" value={numberFormat.format(totalRows)} />
      <Metric label="Storage" value={formatBytes(totalBytes)} />
    </div>

    <Panel title="Buckets" eyebrow="Independent data workspaces" action={<Link className="text-link" to="/data">Manage data <ArrowIcon /></Link>}>
      {(buckets.data?.length ?? 0) ? <div className="home-bucket-list">
        {buckets.data?.map((bucket) => <div className="home-bucket-row" key={bucket.id}>
          <StatusMark active={bucket.ready} />
          <div><strong>{bucket.name}</strong><span>{bucket.is_default ? 'Compatibility bucket · ' : ''}{bucket.id}</span></div>
          <span className="mono">{bucket.ready ? numberFormat.format(bucket.rows) + ' rows' : 'empty'}</span>
          <span className="mono">{bucket.ready ? formatBytes(bucket.total_bytes) : '—'}</span>
        </div>)}
      </div> : <p className="support-copy">No buckets are available yet.</p>}
    </Panel>
  </div>
}
