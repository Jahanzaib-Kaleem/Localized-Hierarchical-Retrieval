import { useMutation, useQueryClient } from '@tanstack/react-query'
import { api, setApiToken } from '../../api/client'
import { Notice, PageHeader, Panel } from '../../components/ui'

export function SettingsPage() {
  const queryClient = useQueryClient()
  const test = useMutation({ mutationFn: () => api.metrics() })

  const logout = async () => {
    setApiToken('')
    queryClient.clear()
  }

  return <div className="page stack stack--lg">
    <PageHeader eyebrow="Security boundary" title="Settings" description="Studio authentication is deliberately session-scoped. The browser never receives a credential from the server and closing the browser session clears the local access secret." />

    <div className="content-grid content-grid--3-2">
      <Panel title="Studio session" eyebrow="Authenticated">
        <div className="stack">
          <p className="support-copy">This browser session is unlocked with an LHR bearer credential. Server-side role checks remain authoritative for every read, mutation, metric and administrative action.</p>
          <div className="button-group">
            <button className="button" disabled={test.isPending} onClick={() => test.mutate()}>{test.isPending ? 'Testing…' : 'Test session'}</button>
            <button className="button button--quiet" onClick={() => void logout()}>Lock Studio</button>
          </div>
          {test.data ? <Notice title="Session accepted">Authenticated metrics request completed.</Notice> : null}
          {test.error ? <Notice title="Session rejected">{test.error.message}</Notice> : null}
        </div>
      </Panel>

      <Panel title="Connection" eyebrow="Runtime">
        <dl className="definition-list">
          <div><dt>Origin</dt><dd className="mono">{window.location.origin}</dd></div>
          <div><dt>API</dt><dd>/v1</dd></div>
          <div><dt>Health</dt><dd>/healthz</dd></div>
          <div><dt>Metrics</dt><dd>/metrics</dd></div>
          <div><dt>MCP</dt><dd>separate authenticated listener</dd></div>
          <div><dt>Frontend server</dt><dd>Rust/Axum in production</dd></div>
        </dl>
      </Panel>
    </div>

    <Panel title="Credential model" eyebrow="Human + machine access">
      <dl className="definition-list">
        <div><dt>Studio</dt><dd>admin access secret held in sessionStorage only</dd></div>
        <div><dt>HTTP API</dt><dd>role-based bearer credentials</dd></div>
        <div><dt>MCP</dt><dd>role-based bearer credentials; never anonymous remotely</dd></div>
        <div><dt>Public deployment</dt><dd>HTTPS/private transport required in front of exposed listeners</dd></div>
        <div><dt>Rotation</dt><dd>restart the appliance with a new LHR_API_TOKEN or rerun the setup wizard</dd></div>
      </dl>
    </Panel>

    <Panel title="Design contract" eyebrow="Studio behavior">
      <dl className="definition-list">
        <div><dt>Palette</dt><dd>#000000 · #151515 · #1e1e1e · #353535 · #e7e7e7</dd></div>
        <div><dt>Reads</dt><dd>bounded and cancelable</dd></div>
        <div><dt>Writes</dt><dd>never retried automatically</dd></div>
        <div><dt>Polling</dt><dd>only on active observability screens</dd></div>
        <div><dt>Correctness</dt><dd>enforced server-side</dd></div>
      </dl>
    </Panel>
  </div>
}
