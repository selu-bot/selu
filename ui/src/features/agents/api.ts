import { apiRequest } from '../../api'

export type Provider = { id: string; display_name: string }
export type ModelSelection = { provider_id: string; model_id: string; temperature?: number }
export type InstalledAgent = { id: string; name: string; version: string; provider_id: string; model_id: string; image_provider_id: string; image_model_id: string; capability_count: number; is_bundled: boolean; setup_complete: boolean; auto_update: boolean; update_available: boolean; marketplace_version: string }
export type MarketplaceAgent = { id: string; name: string; description: string; version: string; author: string; is_installed: boolean; installed_version: string; update_available: boolean; entry_json: string; average_rating?: number; rating_count?: number }
export type AgentsSnapshot = { installed: InstalledAgent[]; marketplace: MarketplaceAgent[]; marketplace_error: boolean; providers: Provider[]; default_model: ModelSelection; default_image_model: ModelSelection }
export type Tool = { capability_id: string; name: string; display_name: string; description: string; policy: string; global_default: string; has_override: boolean }
export type Credential = { capability_id: string; name: string; scope: 'system' | 'user'; description: string; required: boolean; is_set: boolean; set_at?: string }
export type HostPolicy = { host: string; policy: string; source: string; removable: boolean }
export type Capability = { id: string; image_status: string; effective_network_mode: string; network_access_policy: string; host_policies: HostPolicy[]; filesystem: string; max_memory_mb: number; max_cpu_percent: number; pids_limit: number; tools: Tool[]; credentials: Credential[] }
export type StorageEntry = { id: string; user_id: string; key: string; value: string; updated_at: string }
export type MemoryEntry = { id: string; user_id: string; memory: string; tags: string; source: string; updated_at: string }
export type NetworkEntry = { capability_id: string; method: string; host: string; port: number; allowed: boolean; created_at: string }
export type Insight = { id: string; lesson_text: string; insight_type: string; status: string; confidence_percent: number; supporting_signals: number; created_at: string }
export type AgentDetail = {
  id: string; name: string; version: string; provider_id: string; model_id: string; image_provider_id: string; image_model_id: string; temperature: number; is_bundled: boolean; setup_complete: boolean; auto_update: boolean
  overview: { capability_count: number; storage_count: number; memory_count: number; network_request_count: number; permissions_allow_count: number; permissions_ask_count: number; permissions_block_count: number; secrets_set_count: number; secrets_missing_count: number }
  runtime: { autonomy_level: string; use_advanced_limits: boolean; max_tool_loop_iterations: number; max_delegation_hops: number; agent_default_autonomy_level: string; agent_default_max_tool_loop_iterations: number; agent_default_max_delegation_hops: number; has_user_override: boolean }
  automation: { supported: boolean; enabled: boolean; ready: boolean; missing_required_credentials: boolean; missing_default_pipe: boolean; active_schedule_count: number; total_schedule_count: number; presets: Array<{ label: string; cron_description: string }> }
  capabilities: Capability[]; builtin_permissions: Tool[]; storage: StorageEntry[]; memory: MemoryEntry[]; network_log: NetworkEntry[]; improvement: { signal_count: number; insights: Insight[] }
}
export type SetupStep = { id: string; kind: 'input' | 'test'; label: string; description: string; default_value: string; validation: string }
export type SetupPermission = { key: string; capability_id: string; tool_name: string; display_name: string; description: string; recommended: string }
export type AgentSetup = { agent_id: string; agent_name: string; update_flow: boolean; steps: SetupStep[]; permissions: SetupPermission[]; discovery_warning: boolean; providers: Provider[] }
export type UpdateJob = { job_id: string; agent_id: string; agent_name: string; target_version: string; progress: number; message_key: string; done: boolean; success: boolean; redirect_to?: string; error_key?: string }

const json = (method: string, body?: unknown): RequestInit => ({ method, headers: body === undefined ? undefined : { 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) })
const agent = (id: string, suffix = '') => `/api/v1/agents/${encodeURIComponent(id)}${suffix}`

export const agentsApi = {
  list: () => apiRequest<AgentsSnapshot>('/api/v1/agents'),
  detail: (id: string) => apiRequest<AgentDetail>(agent(id)),
  setup: (id: string, update = false) => apiRequest<AgentSetup>(agent(id, `/setup${update ? '?flow=update' : ''}`)),
  install: (entry_json: string) => apiRequest<{ ok: boolean }>('/api/v1/agents/install', json('POST', { entry_json })),
  completeSetup: (id: string, values: Record<string, string>, flow?: string) => apiRequest<void>(agent(id, '/setup'), json('POST', { values, flow })),
  testSetup: (id: string, step: string, values: Record<string, string>) => apiRequest<{ ok: boolean; status?: number }>(agent(id, `/setup/test/${encodeURIComponent(step)}`), json('POST', { values })),
  setModel: (id: string, input: { provider_id: string; model_id: string; temperature?: number }) => apiRequest<void>(agent(id, '/model'), json('PATCH', input)),
  setImageModel: (id: string, provider_id: string, model_id: string) => apiRequest<void>(agent(id, '/image-model'), json('PATCH', { provider_id, model_id })),
  setDefaults: (input: Record<string, unknown>) => apiRequest<void>('/api/v1/agents/defaults', json('PATCH', input)),
  setRuntime: (id: string, input: Record<string, unknown>) => apiRequest<void>(agent(id, '/runtime-settings'), json('PATCH', input)),
  toggleAutomation: (id: string, enabled: boolean) => apiRequest<void>(agent(id, '/automation'), json('PATCH', { enabled })),
  toggleAutoUpdate: (id: string, enabled: boolean) => apiRequest<void>(agent(id, '/auto-update'), json('PATCH', { enabled })),
  setPermission: (id: string, input: { capability_id: string; tool_name: string; policy: string; scope?: string }) => apiRequest<void>(agent(id, '/permissions'), json('PUT', input)),
  resetPermission: (id: string, input: { capability_id: string; tool_name: string }) => apiRequest<void>(agent(id, '/permissions'), json('DELETE', input)),
  setNetworkAccess: (id: string, capability_id: string, access: string) => apiRequest<void>(agent(id, '/network/access'), json('PUT', { capability_id, access })),
  setNetworkHost: (id: string, capability_id: string, host: string, policy: string) => apiRequest<void>(agent(id, '/network/hosts'), json('PUT', { capability_id, host, policy })),
  removeNetworkHost: (id: string, capability_id: string, host: string) => apiRequest<void>(agent(id, '/network/hosts'), json('DELETE', { capability_id, host })),
  setCredential: (id: string, input: { capability_id: string; credential_name: string; scope: string; value: string }) => apiRequest<void>(agent(id, '/credentials'), json('PUT', input)),
  removeCredential: (id: string, credential: Credential) => apiRequest<void>(agent(id, `/credentials/${credential.scope}/${encodeURIComponent(credential.capability_id)}/${encodeURIComponent(credential.name)}`), { method: 'DELETE' }),
  removeStorage: (id: string, entryId: string) => apiRequest<void>(agent(id, `/storage/${encodeURIComponent(entryId)}`), { method: 'DELETE' }),
  removeMemory: (id: string, memoryId: string) => apiRequest<void>(agent(id, `/memory/${encodeURIComponent(memoryId)}`), { method: 'DELETE' }),
  downloadImage: (id: string, capabilityId: string) => apiRequest<void>(agent(id, `/capabilities/${encodeURIComponent(capabilityId)}/image`), json('POST')),
  improvement: (id: string, action: string, insight_id?: string) => apiRequest<void>(agent(id, `/improvement/${action}`), json('POST', { insight_id })),
  rate: (id: string, rating: number) => apiRequest<void>(agent(id, '/rating'), json('PUT', { rating })),
  uninstall: (id: string) => apiRequest<void>(agent(id), { method: 'DELETE' }),
  checkUpdates: () => apiRequest<void>('/api/v1/agents/check-updates', json('POST')),
  startUpdate: (entry_json: string) => apiRequest<{ job_id: string }>('/api/v1/agents/updates', json('POST', { entry_json })),
  updateStatus: (jobId: string) => apiRequest<UpdateJob>(`/api/v1/agents/updates/${encodeURIComponent(jobId)}`),
}
