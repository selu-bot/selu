import { apiRequest } from '../../api'

export type Provider = {
  id: string; display_name: string; kind: 'cloud' | 'local'; requires_api_key: boolean
  requires_base_url: boolean; default_base_url: string | null; configured: boolean
  active: boolean; has_api_key: boolean; base_url: string | null
}
export type ProviderConfiguration = { api_key?: string; base_url?: string }
export type ModelInfo = { id: string; name: string }
const json = (method: string, body?: unknown): RequestInit => ({ method, headers: body === undefined ? undefined : { 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) })
export const connectionsApi = {
  list: () => apiRequest<Provider[]>('/api/v1/providers'),
  configure: (id: string, input: ProviderConfiguration) => apiRequest<Provider>(`/api/v1/providers/${encodeURIComponent(id)}/configuration`, json('PUT', input)),
  remove: (id: string) => apiRequest<void>(`/api/v1/providers/${encodeURIComponent(id)}/configuration`, { method: 'DELETE' }),
  test: (id: string) => apiRequest<{ ok: boolean }>(`/api/v1/providers/${encodeURIComponent(id)}/connection-test`, json('POST')),
  models: (id: string) => apiRequest<ModelInfo[]>(`/api/v1/providers/${encodeURIComponent(id)}/models`),
}
