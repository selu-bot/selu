import { apiRequest } from '../../api'

export type PasswordReceipt = { status: string; other_sessions_revoked: number }
export type PairingToken = { token: string; server_url: string; expires_at: string }
export type SecretMetadata = { scope: 'system' | 'user'; capability_id: string; name: string; user_id?: string; expires_at?: string; created_at: string }
export type CacheVolume = { id: string; capability: string; owner_display_name?: string; last_used: string; size_bytes?: number; state: string }
const json = (method: string, body: unknown): RequestInit => ({ method, headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) })
const segment = encodeURIComponent
export const settingsApi = {
  changePassword: (currentPassword: string, newPassword: string) => apiRequest<PasswordReceipt>('/api/v1/users/me/password', json('PUT', { current_password: currentPassword, new_password: newPassword })),
  createPairingToken: () => apiRequest<PairingToken>('/api/v1/users/me/mobile-pairing-tokens', { method: 'POST' }),
  userSecrets: () => apiRequest<SecretMetadata[]>('/api/v1/secrets/user'),
  putUserSecret: (capability: string, name: string, value: string) => apiRequest<SecretMetadata>(`/api/v1/secrets/user/${segment(capability)}/${segment(name)}`, json('PUT', { value })),
  deleteUserSecret: (capability: string, name: string) => apiRequest<void>(`/api/v1/secrets/user/${segment(capability)}/${segment(name)}`, { method: 'DELETE' }),
  systemSecrets: () => apiRequest<SecretMetadata[]>('/api/v1/secrets/system'),
  putSystemSecret: (capability: string, name: string, value: string) => apiRequest<SecretMetadata>(`/api/v1/secrets/system/${segment(capability)}/${segment(name)}`, json('PUT', { value })),
  deleteSystemSecret: (capability: string, name: string) => apiRequest<void>(`/api/v1/secrets/system/${segment(capability)}/${segment(name)}`, { method: 'DELETE' }),
  cacheVolumes: () => apiRequest<CacheVolume[]>('/api/v1/cache-volumes'),
  deleteCacheVolume: (id: string) => apiRequest<void>(`/api/v1/cache-volumes/${segment(id)}`, { method: 'DELETE' }),
}
