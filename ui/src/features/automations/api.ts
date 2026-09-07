import { apiRequest } from '../../api'

export type DeliveryDestination = { pipe_id: string; name: string; transport: string; active: boolean }
export type AutomationTiming =
  | { type: 'recurring'; cron_expression: string; description: string; timezone: string }
  | { type: 'one_shot'; fire_at: string; description: string; timezone: string }
export type Automation = {
  id: string; name: string; prompt: string; agent_id: string | null; pipe_ids: string[]
  delivery_destinations: DeliveryDestination[]; timing: AutomationTiming; active: boolean
  next_run_at: string; last_run_at: string | null; created_at: string
}
export type TimingInput =
  | { type: 'natural_language'; text: string; timezone?: string }
  | { type: 'cron'; cron_expression: string; description?: string; timezone?: string }
  | { type: 'one_shot'; fire_at: string; description?: string; timezone?: string }
export type AutomationInput = { name: string; prompt: string; agent_id?: string; pipe_ids: string[]; timing: TimingInput }
const json = (method: string, body: unknown): RequestInit => ({ method, headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) })
export const automationsApi = {
  profile: () => apiRequest<{ timezone: string }>('/api/v1/users/me'),
  list: () => apiRequest<{ automations: Automation[] }>('/api/v1/automations'),
  destinations: () => apiRequest<{ destinations: DeliveryDestination[] }>('/api/v1/automation-destinations'),
  create: (input: AutomationInput) => apiRequest<Automation>('/api/v1/automations', json('POST', input)),
  update: (id: string, input: AutomationInput) => apiRequest<Automation>(`/api/v1/automations/${encodeURIComponent(id)}`, json('PUT', input)),
  remove: (id: string) => apiRequest<void>(`/api/v1/automations/${encodeURIComponent(id)}`, { method: 'DELETE' }),
  setActive: (id: string, active: boolean) => apiRequest<Automation>(`/api/v1/automations/${encodeURIComponent(id)}/state`, json('PATCH', { active })),
  setTimezone: (timezone: string) => apiRequest<{ timezone: string }>('/api/v1/users/me/timezone', json('PUT', { timezone })),
}
