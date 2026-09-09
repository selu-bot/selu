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

export type DockerStorageEntryState = 'protected' | 'reclaimable'

export type DockerStorageEntryReason =
  | 'cleanup_blocked'
  | 'container_in_use'
  | 'initial_grace_period'
  | 'current_revision'
  | 'current_system_image'
  | 'previous_revision'
  | 'uninstall_retention'
  | 'staged_or_failed_revision'
  | 'superseded_retention'
  | 'retention_period'
  | 'unreferenced'

export type DockerStorageBlockedCode =
  | ''
  | 'bootstrap_incomplete'
  | 'cleanup_active'
  | 'cleanup_incomplete'
  | 'container_inventory_unavailable'
  | 'docker_unavailable'
  | 'preview_changed'
  | 'restart_helper_active'
  | 'safety_metadata_invalid'
  | 'safety_metadata_stale'
  | 'safety_metadata_unavailable'
  | 'update_active'

export type DockerStorageEntry = {
  image_id: string
  display_name: string
  size_bytes: number
  state: DockerStorageEntryState
  reason: DockerStorageEntryReason
}

export type DockerStorageStatus = {
  managed_bytes: number
  protected_bytes: number
  reclaimable_bytes: number
  managed_image_count: number
  protected_image_count: number
  reclaimable_image_count: number
  last_cleanup_at: string
  blocked_code: DockerStorageBlockedCode
  blocked_reason: string
  entries: DockerStorageEntry[]
}

export type DockerStorageCleanupResult = DockerStorageStatus & {
  reclaimed_bytes: number
  reclaimed_image_count: number
}

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
  storage: () => apiRequest<DockerStorageStatus>('/api/v1/system-updates/storage'),
  cleanupStorage: (imageIds: string[]) => apiRequest<DockerStorageCleanupResult>(
    '/api/v1/system-updates/storage/cleanup',
    json('POST', { image_ids: imageIds }),
  ),
}
