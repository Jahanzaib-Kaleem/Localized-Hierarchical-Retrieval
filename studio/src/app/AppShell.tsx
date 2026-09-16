import { Link, Outlet, useRouterState } from '@tanstack/react-router'
import type { ComponentType, SVGProps } from 'react'
import { DataIcon, GenerationsIcon, IndexIcon, MetricsIcon, OperationsIcon, OverviewIcon, QueryIcon, SettingsIcon, WorkloadIcon } from '../components/icons'

type StudioPath = '/' | '/data' | '/query' | '/indexes' | '/workload' | '/generations' | '/operations' | '/metrics' | '/settings'

const nav: Array<{ to: StudioPath; label: string; icon: ComponentType<SVGProps<SVGSVGElement>> }> = [
  { to: '/', label: 'Overview', icon: OverviewIcon },
  { to: '/data', label: 'Data', icon: DataIcon },
  { to: '/query', label: 'Query', icon: QueryIcon },
  { to: '/indexes', label: 'Indexes', icon: IndexIcon },
  { to: '/workload', label: 'Workload', icon: WorkloadIcon },
  { to: '/generations', label: 'Generations', icon: GenerationsIcon },
  { to: '/operations', label: 'Operations', icon: OperationsIcon },
  { to: '/metrics', label: 'Metrics', icon: MetricsIcon },
  { to: '/settings', label: 'Settings', icon: SettingsIcon },
]

function endpointLabel(): string {
  if (import.meta.env.DEV) return '127.0.0.1:8787 via Vite proxy'
  return window.location.host || 'same origin'
}

export function AppShell() {
  const pathname = useRouterState({ select: (state) => state.location.pathname })

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand" aria-label="LHR Studio">
          <div className="brand__mark">L</div>
          <div className="brand__copy"><strong>LHR</strong><span>Studio</span></div>
        </div>

        <nav className="nav" aria-label="Primary navigation">
          {nav.map(({ to, label, icon: Icon }) => {
            const active = to === '/' ? pathname === '/' : pathname.startsWith(to)
            return (
              <Link className="nav-item" data-active={active ? 'true' : 'false'} key={to} to={to}>
                <Icon className="nav-item__icon" />
                <span>{label}</span>
              </Link>
            )
          })}
        </nav>

        <div className="sidebar__footer">
          <div className="runtime-badge"><span className="status-mark" /><span>control plane</span></div>
          <span className="runtime-version">LHR/1</span>
        </div>
      </aside>

      <main className="main-stage">
        <div className="topbar">
          <div className="topbar__identity"><span className="topbar__label">Active endpoint</span><strong>{endpointLabel()}</strong></div>
          <div className="topbar__meta"><span>Exact database</span><span className="topbar__separator" /><span>Bounded client</span></div>
        </div>
        <div className="main-scroll"><Outlet /></div>
      </main>
    </div>
  )
}
