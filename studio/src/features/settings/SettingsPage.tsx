import { useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { api, getApiToken, setApiToken } from '../../api/client'
import { Notice, PageHeader, Panel } from '../../components/ui'

export function SettingsPage() {
  const queryClient = useQueryClient()
  const [token, setToken] = useState(getApiToken())
  const [saved, setSaved] = useState(false)
  const test = useMutation({ mutationFn: () => api.stats() })
  const save = async () => { setApiToken(token); setSaved(true); await queryClient.invalidateQueries() }
  const clear = async () => { setToken(''); setApiToken(''); setSaved(true); await queryClient.invalidateQueries() }
  return <div className="page stack stack--lg">
    <PageHeader eyebrow="Client boundary" title="Settings" description="Studio keeps connection state intentionally small. Authentication stays in this browser session and all requests remain same-origin." />
    <div className="content-grid content-grid--3-2"><Panel title="API credential" eyebrow="Session storage"><div className="stack"><label className="field"><span>Bearer token</span><input type="password" autoComplete="off" value={token} onChange={(event) => { setToken(event.target.value); setSaved(false) }} placeholder="Leave empty for local-anonymous deployments" /><small>Stored in sessionStorage only. It is cleared when the browser session ends.</small></label><div className="button-group"><button className="button" onClick={() => void save()}>Save for session</button><button className="button button--quiet" onClick={() => void clear()}>Clear</button><button className="button button--quiet" disabled={test.isPending} onClick={() => test.mutate()}>{test.isPending ? 'Testing…' : 'Test connection'}</button></div>{saved ? <Notice title="Session credential updated" /> : null}{test.data ? <Notice title="Connection accepted">Authenticated stats request completed.</Notice> : null}{test.error ? <Notice title="Connection rejected">{test.error.message}</Notice> : null}</div></Panel><Panel title="Connection" eyebrow="Runtime"><dl className="definition-list"><div><dt>Origin</dt><dd className="mono">{window.location.origin}</dd></div><div><dt>API</dt><dd>/v1</dd></div><div><dt>Health</dt><dd>/healthz</dd></div><div><dt>Metrics</dt><dd>/metrics</dd></div><div><dt>Frontend server</dt><dd>Rust/Axum in production</dd></div></dl></Panel></div>
    <Panel title="Design contract" eyebrow="Studio behavior"><dl className="definition-list"><div><dt>Palette</dt><dd>#000000 · #151515 · #1e1e1e · #353535 · #e7e7e7</dd></div><div><dt>Reads</dt><dd>bounded and cancelable</dd></div><div><dt>Writes</dt><dd>never retried automatically</dd></div><div><dt>Polling</dt><dd>only on active observability screens</dd></div><div><dt>Correctness</dt><dd>enforced server-side</dd></div></dl></Panel>
  </div>
}
