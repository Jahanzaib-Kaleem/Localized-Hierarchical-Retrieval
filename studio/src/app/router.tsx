import { createRootRoute, createRoute, createRouter } from '@tanstack/react-router'
import { AppShell } from './AppShell'
import { OverviewPage } from '../features/overview/OverviewPage'
import { EmptyState, Panel } from '../components/ui'

function Placeholder({ title, detail }: { title: string; detail: string }) {
  return (
    <div className="page stack stack--lg">
      <header className="page-header">
        <div className="stack stack--xs"><span className="eyebrow">LHR Studio</span><h1 className="page-title">{title}</h1><p className="page-description">{detail}</p></div>
      </header>
      <Panel title="Surface reserved" eyebrow="Studio foundation"><EmptyState>This surface is wired into navigation but intentionally stays empty until its API behavior is implemented.</EmptyState></Panel>
    </div>
  )
}

const rootRoute = createRootRoute({ component: AppShell })
const overviewRoute = createRoute({ getParentRoute: () => rootRoute, path: '/', component: OverviewPage })
const dataRoute = createRoute({ getParentRoute: () => rootRoute, path: '/data', component: () => <Placeholder title="Data" detail="Browse bounded result sets without ever materializing the database in the browser." /> })
const queryRoute = createRoute({ getParentRoute: () => rootRoute, path: '/query', component: () => <Placeholder title="Query" detail="Build exact predicates, inspect execution cost, and paginate by stable logical row ID." /> })
const indexesRoute = createRoute({ getParentRoute: () => rootRoute, path: '/indexes', component: () => <Placeholder title="Indexes" detail="Inspect and administer exact routing structures without changing logical correctness." /> })
const workloadRoute = createRoute({ getParentRoute: () => rootRoute, path: '/workload', component: () => <Placeholder title="Workload" detail="Read persistent latency and query-shape telemetry with deliberate, low-frequency refreshes." /> })
const generationsRoute = createRoute({ getParentRoute: () => rootRoute, path: '/generations', component: () => <Placeholder title="Generations" detail="Inspect immutable generations, leases, rollback posture, and retention." /> })
const operationsRoute = createRoute({ getParentRoute: () => rootRoute, path: '/operations', component: () => <Placeholder title="Operations" detail="Compaction, recovery, and vacuum controls live here behind explicit admin actions." /> })
const metricsRoute = createRoute({ getParentRoute: () => rootRoute, path: '/metrics', component: () => <Placeholder title="Metrics" detail="Process memory, page faults, I/O, requests, and database counters with no decorative chart noise." /> })
const settingsRoute = createRoute({ getParentRoute: () => rootRoute, path: '/settings', component: () => <Placeholder title="Settings" detail="Connection credentials and Studio-local behavior, stored only for the current browser session." /> })

const routeTree = rootRoute.addChildren([overviewRoute, dataRoute, queryRoute, indexesRoute, workloadRoute, generationsRoute, operationsRoute, metricsRoute, settingsRoute])

export const router = createRouter({ routeTree, defaultPreload: 'intent' })

declare module '@tanstack/react-router' {
  interface Register { router: typeof router }
}
