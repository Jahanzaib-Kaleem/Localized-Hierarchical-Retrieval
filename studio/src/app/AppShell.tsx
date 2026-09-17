import { Link, Outlet, useRouterState } from '@tanstack/react-router'
import { useQuery } from '@tanstack/react-query'
import type { ComponentType, SVGProps } from 'react'
import { api } from '../api/client'
import { formatBytes, numberFormat } from '../components/format'
import { ApiIcon, DataIcon, HomeIcon, QueryIcon, SettingsIcon, SystemIcon } from '../components/icons'
import { StatusMark } from '../components/ui'

type StudioPath = '/' | '/data' | '/query' | '/api' | '/system' | '/settings'

const primaryNav: Array<{ to: StudioPath; label: string; icon: ComponentType<SVGProps<SVGSVGElement>> }> = [
  { to: '/', label: 'Home', icon: HomeIcon },
  { to: '/data', label: 'Data', icon: DataIcon },
  { to: '/query', label: 'Query', icon: QueryIcon },
  { to: '/api', label: 'API', icon: ApiIcon },
]

const systemPaths = ['/system', '/indexes', '/workload', '/generations', '/operations', '/metrics']

function currentLabel(pathname: string) {
  if (pathname === '/') return 'Home'
  if (pathname.startsWith('/data')) return 'Data'
  if (pathname.startsWith('/query')) return 'Query'
  if (pathname.startsWith('/api')) return 'API'
  if (pathname.startsWith('/settings')) return 'Settings'
  if (systemPaths.some((path) => pathname.startsWith(path))) return 'System'
  return 'LHR Studio'
}

export function AppShell() {
  const pathname = useRouterState({ select: (state) => state.location.pathname })
  const ready = useQuery({ queryKey: ['ready'], queryFn: ({ signal }) => api.ready(signal), staleTime: 10_000, refetchInterval: 30_000 })
  const stats = useQuery({ queryKey: ['stats'], queryFn: ({ signal }) => api.stats(signal), staleTime: 30_000, retry: false })
  const isReady = ready.data?.status === 'ready'
  const systemActive = systemPaths.some((path) => pathname.startsWith(path))

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <Link className="brand" to="/" aria-label="LHR Studio home">
          <div className="brand__mark">L</div>
          <div className="brand__copy"><strong>LHR</strong><span>Studio</span></div>
        </Link>

        <nav className="nav" aria-label="Primary navigation">
          {primaryNav.map(({ to, label, icon: Icon }) => {
            const active = to === '/' ? pathname === '/' : pathname.startsWith(to)
            return (
              <Link className="nav-item" data-active={active ? 'true' : 'false'} key={to} to={to}>
                <Icon className="nav-item__icon" />
                <span>{label}</span>
              </Link>
            )
          })}
        </nav>

        <div className="nav nav--secondary">
          <Link className="nav-item" data-active={systemActive ? 'true' : 'false'} to="/system">
            <SystemIcon className="nav-item__icon" /><span>System</span>
          </Link>
          <Link className="nav-item" data-active={pathname.startsWith('/settings') ? 'true' : 'false'} to="/settings">
            <SettingsIcon className="nav-item__icon" /><span>Settings</span>
          </Link>
        </div>

        <div className="sidebar__footer">
          <div className="runtime-badge"><StatusMark active={isReady} /><span>{isReady ? 'Ready' : 'No dataset'}</span></div>
          <span className="runtime-version">LHR/1</span>
        </div>
      </aside>

      <main className="main-stage">
        <div className="topbar">
          <strong className="topbar__page">{currentLabel(pathname)}</strong>
          <div className="topbar__diagnostics" aria-label="Dataset status">
            <span className="topbar__state"><StatusMark active={isReady} />{isReady ? 'Ready' : 'Not ready'}</span>
            {stats.data ? <><span className="topbar__separator" /><span><strong>{numberFormat.format(stats.data.rows)}</strong> rows</span><span className="topbar__separator" /><span><strong>{formatBytes(stats.data.total_bytes)}</strong></span></> : null}
          </div>
        </div>
        <div className="main-scroll"><Outlet /></div>
      </main>
    </div>
  )
}
