import { useEffect, useMemo, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { api } from '../../api/client'
import { CopyIcon } from '../../components/icons'
import { PageHeader, Panel } from '../../components/ui'

function CopyButton({ value }: { value: string }) {
  const [copied, setCopied] = useState(false)
  const copy = async () => {
    await navigator.clipboard.writeText(value)
    setCopied(true)
    window.setTimeout(() => setCopied(false), 1200)
  }
  return <button className="button button--quiet" type="button" onClick={() => void copy()}><CopyIcon className="button__icon" />{copied ? 'Copied' : 'Copy'}</button>
}

export function ApiPage() {
  const buckets = useQuery({ queryKey: ['buckets'], queryFn: ({ signal }) => api.buckets(signal), staleTime: 15_000 })
  const [bucket, setBucket] = useState('default')
  const activeBucket = buckets.data?.find((item) => item.id === bucket)
  useEffect(() => {
    if (buckets.data?.length && !buckets.data.some((item) => item.id === bucket && item.ready)) {
      const firstReady = buckets.data.find((item) => item.ready)
      if (firstReady) setBucket(firstReady.id)
    }
  }, [buckets.data, bucket])
  const stats = useQuery({ queryKey: ['stats', bucket], queryFn: ({ signal }) => api.stats(signal, bucket), staleTime: 30_000, retry: false, enabled: Boolean(activeBucket?.ready) })
  const firstColumn = stats.data?.column_stats[0]?.name ?? 'domain'
  const origin = window.location.origin
  const body = useMemo(() => JSON.stringify({ bucket, filters: [{ op: 'eq', column: firstColumn, value: 'example' }], limit: 100 }, null, 2), [bucket, firstColumn])
  const curl = `curl ${origin}/v1/query \\\n  -H "Authorization: Bearer $LHR_API_TOKEN" \\\n  -H "Content-Type: application/json" \\\n  -d '${body.replace(/\n/g, '')}'`
  const fetchExample = `const response = await fetch('${origin}/v1/query', {\n  method: 'POST',\n  headers: {\n    Authorization: \`Bearer \${process.env.LHR_API_TOKEN}\`,\n    'Content-Type': 'application/json',\n  },\n  body: JSON.stringify(${body}),\n})\n\nconst { result } = await response.json()`

  return <div className="page stack stack--lg">
    <PageHeader eyebrow="Developer access" title="API" description="Use the same exact bucket-aware query engine from your application. Copy a working request, then adapt the filters and projection to your schema." action={buckets.data?.length ? <label className="bucket-select"><span>Example bucket</span><select value={bucket} onChange={(event) => setBucket(event.target.value)}>{buckets.data.map((item) => <option key={item.id} value={item.id} disabled={!item.ready}>{item.name}{item.ready ? '' : ' · empty'}</option>)}</select></label> : undefined} />

    <div className="content-grid content-grid--3-2">
      <Panel title="Quick start" eyebrow="HTTP">
        <div className="stack">
          <div className="code-block__header"><span>cURL</span><CopyButton value={curl} /></div>
          <pre className="code-block">{curl}</pre>
          <div className="code-block__header"><span>JavaScript</span><CopyButton value={fetchExample} /></div>
          <pre className="code-block">{fetchExample}</pre>
        </div>
      </Panel>
      <Panel title="Connection" eyebrow="Current Studio">
        <dl className="definition-list definition-list--roomy">
          <div><dt>Base URL</dt><dd className="mono">{origin}</dd></div>
          <div><dt>Authentication</dt><dd>Bearer token</dd></div>
          <div><dt>Response envelope</dt><dd className="mono">request_id + result</dd></div>
          <div><dt>Bucket</dt><dd className="mono">{bucket}</dd></div>
          <div><dt>Dataset</dt><dd>{stats.data ? `${stats.data.rows.toLocaleString()} rows` : 'not available'}</dd></div>
        </dl>
        <p className="support-copy api-note">Keep API tokens server-side in your application. Studio stores its token only for the current browser session.</p>
      </Panel>
    </div>

    <Panel title="Core endpoints" eyebrow="What you will actually use">
      <div className="endpoint-list">
        <div className="endpoint-row"><span className="method-chip">GET</span><code>/v1/buckets</code><p>List the default compatibility bucket and named data buckets.</p></div>
        <div className="endpoint-row"><span className="method-chip">POST</span><code>/v1/query</code><p>Bucket-aware browsing, equality, set, and numeric range queries with cursor pagination.</p></div>
        <div className="endpoint-row"><span className="method-chip">GET</span><code>/v1/stats</code><p>Schema, row count, storage footprint, and index metadata.</p></div>
        <div className="endpoint-row"><span className="method-chip">POST</span><code>/v1/mutate</code><p>Transactional insert, update, and delete operations.</p></div>
        <div className="endpoint-row"><span className="method-chip">GET</span><code>/healthz</code><p>Lightweight process health for deployment checks.</p></div>
        <div className="endpoint-row"><span className="method-chip">GET</span><code>/readyz</code><p>Whether a published dataset is available for queries.</p></div>
      </div>
    </Panel>
  </div>
}
