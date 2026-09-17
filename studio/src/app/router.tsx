import { createRootRoute, createRoute, createRouter } from '@tanstack/react-router'
import { AppShell } from './AppShell'
import { OverviewPage } from '../features/overview/OverviewPage'
import { DataPage } from '../features/data/DataPage'
import { QueryPage } from '../features/query/QueryPage'
import { ApiPage } from '../features/api/ApiPage'
import { SystemPage } from '../features/system/SystemPage'
import { IndexesPage } from '../features/indexes/IndexesPage'
import { WorkloadPage } from '../features/workload/WorkloadPage'
import { GenerationsPage } from '../features/generations/GenerationsPage'
import { OperationsPage } from '../features/operations/OperationsPage'
import { MetricsPage } from '../features/metrics/MetricsPage'
import { SettingsPage } from '../features/settings/SettingsPage'

const rootRoute = createRootRoute({ component: AppShell })
const overviewRoute = createRoute({ getParentRoute: () => rootRoute, path: '/', component: OverviewPage })
const dataRoute = createRoute({ getParentRoute: () => rootRoute, path: '/data', component: DataPage })
const queryRoute = createRoute({ getParentRoute: () => rootRoute, path: '/query', component: QueryPage })
const apiRoute = createRoute({ getParentRoute: () => rootRoute, path: '/api', component: ApiPage })
const systemRoute = createRoute({ getParentRoute: () => rootRoute, path: '/system', component: SystemPage })
const indexesRoute = createRoute({ getParentRoute: () => rootRoute, path: '/indexes', component: IndexesPage })
const workloadRoute = createRoute({ getParentRoute: () => rootRoute, path: '/workload', component: WorkloadPage })
const generationsRoute = createRoute({ getParentRoute: () => rootRoute, path: '/generations', component: GenerationsPage })
const operationsRoute = createRoute({ getParentRoute: () => rootRoute, path: '/operations', component: OperationsPage })
const metricsRoute = createRoute({ getParentRoute: () => rootRoute, path: '/metrics', component: MetricsPage })
const settingsRoute = createRoute({ getParentRoute: () => rootRoute, path: '/settings', component: SettingsPage })
const routeTree = rootRoute.addChildren([
  overviewRoute, dataRoute, queryRoute, apiRoute, systemRoute,
  indexesRoute, workloadRoute, generationsRoute, operationsRoute, metricsRoute, settingsRoute,
])
export const router = createRouter({ routeTree, defaultPreload: 'intent' })
declare module '@tanstack/react-router' { interface Register { router: typeof router } }
