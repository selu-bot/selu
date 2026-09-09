import { Outlet, createRootRouteWithContext, createRoute, createRouter, lazyRouteComponent, redirect } from '@tanstack/react-router'
import type { QueryClient } from '@tanstack/react-query'
import type { AuthState } from '../api'
import { t } from '../i18n'
import { getBasePath } from '../shared/paths'
import { LoginPage, SetupPage, authStateQuery } from '../features/auth/AuthPages'
import { HomePage } from '../features/home/HomePage'
import { PastDaysPage, SavedTopicsPage } from '../features/home/ConversationArchivePage'
import { ChatPage } from '../features/chat/ChatPage'

export type AuthDestination = '/app' | '/app/login' | '/app/setup' | null

export function resolveAuthRedirect(status: AuthState['status'], pathname: string): AuthDestination {
  const isLogin = pathname.endsWith('/app/login')
  const isSetup = pathname.endsWith('/app/setup')
  if (status === 'setup_required') return isSetup ? null : '/app/setup'
  if (status === 'anonymous') return isLogin ? null : '/app/login'
  return isLogin || isSetup ? '/app' : null
}

type RouterContext = { queryClient: QueryClient }
const rootRoute = createRootRouteWithContext<RouterContext>()({
  component: Outlet,
  errorComponent: ({ error }) => <RouteError error={error} />,
  notFoundComponent: () => <RouteError error={new Error(t('pageNotFound'))} />,
})
const appRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: 'app',
  component: Outlet,
  pendingComponent: RouteLoading,
  beforeLoad: async ({ context, location }) => {
    const auth = await context.queryClient.ensureQueryData(authStateQuery)
    const destination = resolveAuthRedirect(auth.status, location.pathname)
    if (destination) throw redirect({ to: destination, replace: true })
    return { auth }
  },
})
const homeRoute = createRoute({ getParentRoute: () => appRoute, path: '/', component: HomePage })
const savedRoute = createRoute({ getParentRoute: () => appRoute, path: 'saved', component: SavedTopicsPage })
const pastRoute = createRoute({ getParentRoute: () => appRoute, path: 'past', component: PastDaysPage })
const loginRoute = createRoute({ getParentRoute: () => appRoute, path: 'login', component: LoginPage })
const setupRoute = createRoute({ getParentRoute: () => appRoute, path: 'setup', component: SetupPage })
const conversationsRoute = createRoute({ getParentRoute: () => appRoute, path: 'conversations', component: () => <ChatPage conversationId={null} /> })
const automationsRoute = createRoute({ getParentRoute: () => appRoute, path: 'automations', component: lazyRouteComponent(() => import('../features/automations/AutomationsPage'), 'AutomationsPage') })
const agentsRoute = createRoute({ getParentRoute: () => appRoute, path: 'agents', component: lazyRouteComponent(() => import('../features/agents/AgentsPage'), 'AgentsPage') })
const agentDetailRoute = createRoute({ getParentRoute: () => appRoute, path: 'agents/$agentId', component: lazyRouteComponent(() => import('../features/agents/AgentsPage'), 'AgentDetailPage') })
const connectorsRoute = createRoute({ getParentRoute: () => appRoute, path: 'connectors', component: lazyRouteComponent(() => import('../features/connectors/ConnectorsPage'), 'ConnectorsPage') })
const updatesRoute = createRoute({ getParentRoute: () => appRoute, path: 'updates', component: lazyRouteComponent(() => import('../features/updates/UpdatesPage'), 'UpdatesPage') })
const connectionsRoute = createRoute({ getParentRoute: () => appRoute, path: 'connections', component: lazyRouteComponent(() => import('../features/connections/ConnectionsPage'), 'ConnectionsPage') })
const aboutRoute = createRoute({ getParentRoute: () => appRoute, path: 'about-you', component: lazyRouteComponent(() => import('../features/about/AboutPage'), 'AboutPage') })
const peopleRoute = createRoute({ getParentRoute: () => appRoute, path: 'people', component: lazyRouteComponent(() => import('../features/people/PeoplePage'), 'PeoplePage') })
const settingsRoute = createRoute({ getParentRoute: () => appRoute, path: 'settings', component: lazyRouteComponent(() => import('../features/settings/SettingsPage'), 'SettingsPage') })
const feedbackRoute = createRoute({ getParentRoute: () => appRoute, path: 'feedback', component: lazyRouteComponent(() => import('../features/feedback/FeedbackPage'), 'FeedbackPage') })
const conversationRoute = createRoute({
  getParentRoute: () => appRoute,
  path: 'conversations/$conversationId',
  component: ConversationRoute,
})
function ConversationRoute() {
  const { conversationId } = conversationRoute.useParams()
  return <ChatPage conversationId={conversationId} />
}

export const routeTree = rootRoute.addChildren([
  appRoute.addChildren([
    homeRoute, savedRoute, pastRoute, loginRoute, setupRoute, conversationsRoute, conversationRoute,
    automationsRoute, agentsRoute, agentDetailRoute, connectorsRoute, updatesRoute, connectionsRoute, aboutRoute, peopleRoute, settingsRoute, feedbackRoute,
  ]),
])

export function createAppRouter(queryClient: QueryClient, basepath = getBasePath()) {
  return createRouter({ routeTree, context: { queryClient }, basepath, defaultPreload: 'intent', defaultPendingMs: 120 })
}

function RouteLoading() {
  return <main className="route-state" aria-busy="true"><span className="route-spinner" /><h1>{t('loading')}</h1><p>{t('loadingPage')}</p></main>
}

function RouteError({ error }: { error: Error }) {
  return <main className="route-state" role="alert"><span className="route-error-mark">!</span><h1>{t('somethingWentWrong')}</h1><p>{error.message || t('tryAgain')}</p><button onClick={() => window.location.reload()}>{t('tryAgainAction')}</button></main>
}

declare module '@tanstack/react-router' {
  interface Register { router: ReturnType<typeof createAppRouter> }
}
