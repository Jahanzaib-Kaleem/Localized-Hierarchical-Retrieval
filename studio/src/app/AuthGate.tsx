import { type FormEvent, type ReactNode, useCallback, useEffect, useState } from 'react'
import { ApiError, api, getApiToken, setApiToken } from '../api/client'

type GateState = 'checking' | 'locked' | 'ready'

export function AuthGate({ children }: { children: ReactNode }) {
  const [state, setState] = useState<GateState>('checking')
  const [secret, setSecret] = useState('')
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)

  const validate = useCallback(async () => {
    try {
      await api.metrics()
      setState('ready')
      setError('')
    } catch (reason) {
      if (reason instanceof ApiError && (reason.status === 401 || reason.status === 403)) {
        if (getApiToken()) setApiToken('')
        setState('locked')
        setError('')
        return
      }
      setState('locked')
      setError(reason instanceof Error ? reason.message : 'Unable to reach LHR.')
    }
  }, [])

  useEffect(() => {
    void validate()
    const onTokenChange = () => {
      if (!getApiToken()) void validate()
    }
    window.addEventListener('lhr-token-change', onTokenChange)
    return () => window.removeEventListener('lhr-token-change', onTokenChange)
  }, [validate])

  const submit = async (event: FormEvent) => {
    event.preventDefault()
    const value = secret.trim()
    if (value.length < 16) {
      setError('Access tokens must contain at least 16 characters.')
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
      setError(reason instanceof Error ? reason.message : 'The access token was rejected.')
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
          <span className="eyebrow">Secure access</span>
          <h1>{state === 'checking' ? 'Connecting to LHR' : 'Enter your access token'}</h1>
          <p className="support-copy">Studio uses the same permissions as the LHR API. The token stays in this browser session and is cleared when you lock Studio or close the session.</p>
        </header>

        {state === 'checking' ? <div className="auth-check mono">CONNECTING</div> : <form className="stack" onSubmit={(event) => void submit(event)}>
          <label className="field">
            <span>Access token</span>
            <input
              type="password"
              autoComplete="current-password"
              autoFocus
              value={secret}
              onChange={(event) => setSecret(event.target.value)}
              placeholder="Paste your LHR API token"
            />
            <small>Use an admin token if you want to import data or use System maintenance tools.</small>
          </label>
          {error ? <div className="auth-error" role="alert">{error}</div> : null}
          <button className="button button--primary auth-submit" type="submit" disabled={busy}>{busy ? 'Connecting…' : 'Open Studio'}</button>
        </form>}

        <div className="auth-help">
          <span className="mono">FIRST RUN</span>
          <p>The standard Docker setup stores its generated admin token inside the container:</p>
          <code>docker exec lhr cat /data/.lhr-admin-token</code>
          <p>Local loopback deployments without configured API keys open Studio directly and do not require this step.</p>
        </div>
      </div>
    </section>
  </main>
}
