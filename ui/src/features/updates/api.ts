import { apiRequest } from '../../api'

export type UpdateSettings = {
  release_channel: string
  auto_check: boolean
  auto_update: boolean
  installation_telemetry_opt_out: boolean
  public_origin: string
  current_origin: string
  push_notifications_enabled: boolean
  available_channels: Array<{ value: string; label: string }>
}

export type UpdateStatus = {
  active_job_id: string
  status: string
  progress_key: string
  last_error: string
  last_checked_at: string
  last_attempt_at: string
  installed_version: string
  installed_display: string
  installed_release_version: string
  installed_build_number: string
  available_version: string
  available_display: string
  available_release_version: string
  available_build_number: string
  available_changelog_url: string
  available_changelog_body: string
  previous_version: string
  previous_display: string
  previous_release_version: string
  previous_build_number: string
  update_available: boolean
  rollback_available: boolean
}

export type UpdateSettingsInput = Partial<Pick<UpdateSettings,
  'release_channel' | 'auto_update' | 'installation_telemetry_opt_out' | 'push_notifications_enabled' | 'public_origin'
>>

const json = (method: string, body?: unknown): RequestInit => ({
  method,
  headers: body === undefined ? undefined : { 'Content-Type': 'application/json' },
  body: body === undefined ? undefined : JSON.stringify(body),
})

export const updatesApi = {
  settings: () => apiRequest<UpdateSettings>('/api/v1/system-updates'),
  status: () => apiRequest<UpdateStatus>('/api/v1/system-updates/status'),
  save: (input: UpdateSettingsInput) => apiRequest<void>('/api/v1/system-updates/settings', json('PATCH', input)),
  check: () => apiRequest<{ ok: boolean; message_key: string }>('/api/v1/system-updates/check', json('POST')),
  apply: () => apiRequest<{ ok: boolean; message_key?: string }>('/api/v1/system-updates/apply', json('POST')),
  rollback: () => apiRequest<{ ok: boolean; message_key?: string }>('/api/v1/system-updates/rollback', json('POST')),
}
