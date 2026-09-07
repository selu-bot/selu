import { apiRequest } from '../../api'

export type UserAccount = {
  id: string
  username: string
  display_name: string
  is_admin: boolean
  language: string
  timezone: string
  created_at: string
  allowed_agent_ids: string[]
}

export type ProfileFact = { id: string; fact: string; category: string; source: string; updated_at: string }
export type ProfileUpdate = { display_name?: string; language?: 'en' | 'de'; timezone?: string }
export type FactInput = { fact: string; category?: string }
const json = (method: string, body: unknown): RequestInit => ({ method, headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) })

export const aboutApi = {
  me: () => apiRequest<UserAccount>('/api/v1/users/me'),
  updateMe: (input: ProfileUpdate) => apiRequest<UserAccount>('/api/v1/users/me', json('PATCH', input)),
  facts: () => apiRequest<ProfileFact[]>('/api/v1/users/me/profile-facts'),
  createFact: (input: FactInput) => apiRequest<{ id: string }>('/api/v1/users/me/profile-facts', json('POST', input)),
  updateFact: (id: string, input: FactInput) => apiRequest<void>(`/api/v1/users/me/profile-facts/${encodeURIComponent(id)}`, json('PUT', input)),
  deleteFact: (id: string) => apiRequest<void>(`/api/v1/users/me/profile-facts/${encodeURIComponent(id)}`, { method: 'DELETE' }),
}
