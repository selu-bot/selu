import { apiRequest } from '../../api'

export type Person = {
  id: string
  username: string
  display_name: string
  is_admin: boolean
  language: string
  timezone: string
  created_at: string
  allowed_agent_ids: string[]
}
export type CreatePerson = { username: string; display_name: string; password: string; is_admin: boolean; language: 'en' | 'de'; timezone: string }
export type UpdatePerson = Partial<Omit<CreatePerson, 'password'>>
const json = (method: string, body: unknown): RequestInit => ({ method, headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) })
export const peopleApi = {
  list: () => apiRequest<Person[]>('/api/v1/users'),
  create: (input: CreatePerson) => apiRequest<Person>('/api/v1/users', json('POST', input)),
  update: (id: string, input: UpdatePerson) => apiRequest<Person>(`/api/v1/users/${encodeURIComponent(id)}`, json('PATCH', input)),
  remove: (id: string) => apiRequest<void>(`/api/v1/users/${encodeURIComponent(id)}`, { method: 'DELETE' }),
  setAgentAccess: (id: string, agentIds: string[]) => apiRequest<{ user_id: string; allowed_agent_ids: string[] }>(`/api/v1/users/${encodeURIComponent(id)}/agent-access`, json('PUT', { agent_ids: agentIds })),
}
