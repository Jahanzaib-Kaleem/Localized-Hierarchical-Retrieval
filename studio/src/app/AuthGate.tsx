import { type FormEvent, type ReactNode, useCallback, useEffect, useState } from 'react'
import { ApiError, api, getApiToken, setApiToken } from '../api/client'

type GateState = 'checking' | 'locked' | 'ready'

export function AuthGate({ children }: { children: ReactNode }) {
  const [state, setState] = useState<GateState>('checking')
  const [secret, setSecret] = useState('')
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)

  const validate = useCallback(async () => {
    const token = getApiToken()
    if (!token) {
      setState('locked')
      return
    }
    try {
      await api.metrics()
      setState('ready')
      setError('')
    } catch (reason) {
      if (reason instanceof ApiError && (reason.status === 401 || reason.status === 403)) {
        setApiToken('')
        setState('locked')
        setError('The access secret was rejected.')
        return
      }
      setState('locked')
      setError(reason instanceof Error ? reason.message : 'Unable to reach LHR.')
    }
  }, [])

  useEffect(() => {
    void validate()
    const onTokenChange = () => {
      if (!getApiToken()) {
        setSecret('')
        setError('')
        setState('locked')
      }
    }
    window.addEventListener('lhr-token-change', onTokenChange)
    return () => window.removeEventListener('lhr-token-change', onTokenChange)
  }, [validate])

  const submit = async (event: FormEvent) => {
    event.preventDefault()
    const value = secret.trim()
    if (value.length < 16) {
      setError('Access secrets must contain at least 16 characters.')
      return
    }
    setBusy(true)
    setError('')
    setApiToken(value)
    try {
      await api.metrics()
      setSecret('')
      setState('ready')
    } catch (reason) {
      setApiToken('')
      setError(reason instanceof Error ? reason.message : 'The access secret was rejected.')
      setState('locked')
    } finally {
      setBusy(false)
    }
  }

  if (state === 'ready') return <>{children}</>

  return <main className="auth-screen">
    <section className="auth-panel" aria-busy={state === 'checking' || busy}>
      <div className="auth-mark" aria-hidden="true">LHR</div>
      <div className="stack stack--lg">
        <header className="stack">
          <span className="eyebrow">Restricted control plane</span>
          <h1>{state === 'checking' ? 'Verifying session' : 'Unlock LHR Studio'}</h1>
          <p className="support-copy">The Studio shell contains no dataset content. Database reads, mutations, metrics and administration remain server-side protected until a valid administrator credential is supplied.</p>
        </header>

        {state === 'checking' ? <div className="auth-check mono">AUTH / VERIFYING</div> : <form className="stack" onSubmit={(event) => void submit(event)}>
          <label className="field">
            <span>Administrator access secret</span>
            <input
              type="password"
              autoComplete="current-password"
              autoFocus
              value={secret}
              onChange={(event) => setSecret(event.target.value)}
              placeholder="Enter the secret configured at deployment"
            />
            <small>Held only in this browser session. Closing the session clears it.</small>
          </label>
          {error ? <div className="auth-error" role="alert">{error}</div> : null}
          <button className="button auth-submit" type="submit" disabled={busy}>{busy ? 'Verifying…' : 'Unlock Studio'}</button>
        </form>}

        <div className="auth-help">
          <span className="mono">FIRST RUN</span>
          <p>With the standard Docker quick start, retrieve the generated secret locally:</p>
          <code>docker exec lhr cat /data/.lhr-admin-token</code>
          <p>Or use the included PowerShell/Bash setup wizard to choose the secret before the container starts.</p>
        </div>
      </div>
    </section>
  </main>
}
