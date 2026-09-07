import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { RouterProvider } from '@tanstack/react-router'
import { NoticeProvider } from './notices'
import { createAppRouter } from './app/router'
import './style.css'
import './styles/primitives.css'

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: { staleTime: 10_000, retry: 1, refetchOnWindowFocus: true },
    mutations: { retry: 0 },
  },
})

const router = createAppRouter(queryClient)

export default function App() {
  return <QueryClientProvider client={queryClient}><NoticeProvider><RouterProvider router={router} /></NoticeProvider></QueryClientProvider>
}
