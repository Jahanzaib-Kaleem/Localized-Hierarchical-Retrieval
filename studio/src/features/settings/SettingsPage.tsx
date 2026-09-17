import { Link } from '@tanstack/react-router'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, setApiToken } from '../../api/client'
import { ArrowIcon } from '../../components/icons'
import { Notice, PageHeader, Panel, StatusMark } from '../../components/ui'

export function SettingsPage() {
  const queryClient = useQueryClient()
  const ready = useQuery({ queryKey: ['ready'], queryFn: ({ signal }) => api.ready(signal), staleTime: 10_000 })
  const test = useMutation({ mutationFn: () => api.stats() })
  const isReady = ready.data?.status === 'ready'

  const logout = async () => {
    setApiToken('')
    queryClient.clear()
  }

  return <div className="page stack stack--lg">
    <PageHeader eyebrow="Studio" title="Settings" description="Manage this browser session and see where Studio is connected." />

    <div className="content-grid content-grid--3-2">
      <Panel title="Session" eyebrow="Authentication">
        <div className="stack">
          <div className="session-state"><StatusMark active /><div><strong>Studio unlocked</strong><span>Your bearer token is stored only in this browser session.</span></div></div>
          <div className="button-group">
            <button className="button" disabled={test.isPending} onClick={() => test.mutate()}>{test.isPending ? 'Testing…' : 'Test connection'}</button>
            <button className="button button--quiet" onClick={() => void logout()}>Lock Studio</button>
          </div>
          {test.data ? <Notice title="Connection works">Authenticated dataset request completed.</Notice> : null}
          {test.error ? <Notice title="Connection failed">{test.error.message}</Notice> : null}
        </div>
      </Panel>

      <Panel title="Connection" eyebrow="Current origin">
        <dl className="definition-list definition-list--roomy">
          <div><dt>Status</dt><dd>{isReady ? 'Dataset ready' : 'Service connected'}</dd></div>
          <div><dt>Origin</dt><dd className="mono text-truncate" title={window.location.origin}>{window.location.origin}</dd></div>
          <div><dt>API base</dt><dd className="mono">/v1</dd></div>
        </dl>
        <Link className="text-link settings-api-link" to="/api">Open API quick start <ArrowIcon /></Link>
      </Panel>
    </div>
  </div>
}
