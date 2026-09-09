import { afterEach, describe, expect, it, vi } from 'vitest'
import { updatesApi, type DockerStorageCleanupResult, type DockerStorageStatus } from './api'

const protectedEntries: DockerStorageStatus['entries'] = [
  { image_id: 'sha256:current', display_name: 'Current image', size_bytes: 512, state: 'protected', reason: 'current_revision' },
  { image_id: 'sha256:rollback', display_name: 'Rollback image', size_bytes: 256, state: 'protected', reason: 'previous_revision' },
]

const storage: DockerStorageStatus = {
  managed_bytes: 1024,
  protected_bytes: 768,
  reclaimable_bytes: 256,
  managed_image_count: 3,
  protected_image_count: 2,
  reclaimable_image_count: 1,
  last_cleanup_at: '',
  blocked_code: '',
  blocked_reason: '',
  entries: [
    ...protectedEntries,
    { image_id: 'sha256:old', display_name: 'Old image', size_bytes: 256, state: 'reclaimable', reason: 'unreferenced' },
  ],
}

const cleanup: DockerStorageCleanupResult = {
  ...storage,
  managed_bytes: 768,
  reclaimable_bytes: 0,
  managed_image_count: 2,
  reclaimable_image_count: 0,
  entries: protectedEntries,
  reclaimed_bytes: 256,
  reclaimed_image_count: 1,
}

afterEach(() => vi.unstubAllGlobals())

function stubDocument() {
  vi.stubGlobal('document', { querySelector: () => null })
}

function jsonResponse(value: unknown, status = 200) {
  return new Response(JSON.stringify(value), { status, headers: { 'Content-Type': 'application/json' } })
}

describe('Docker storage API', () => {
  it('loads the typed storage status from the dedicated endpoint', async () => {
    stubDocument()
    const fetch = vi.fn(async () => jsonResponse(storage))
    vi.stubGlobal('fetch', fetch)

    await expect(updatesApi.storage()).resolves.toEqual(storage)
    expect(fetch).toHaveBeenCalledWith('/api/v1/system-updates/storage', expect.objectContaining({ credentials: 'same-origin' }))
  })

  it('posts the exact reviewed image IDs and returns consistent reclaimed totals', async () => {
    stubDocument()
    const fetch = vi.fn(async () => jsonResponse(cleanup))
    vi.stubGlobal('fetch', fetch)

    await expect(updatesApi.cleanupStorage(['sha256:old'])).resolves.toEqual(cleanup)
    expect(fetch).toHaveBeenCalledWith('/api/v1/system-updates/storage/cleanup', expect.objectContaining({
      method: 'POST',
      body: JSON.stringify({ image_ids: ['sha256:old'] }),
    }))
    expect(cleanup.entries.reduce((total, entry) => total + entry.size_bytes, 0)).toBe(cleanup.managed_bytes)
    expect(cleanup.protected_bytes + cleanup.reclaimable_bytes).toBe(cleanup.managed_bytes)
  })

  it('surfaces the structured backend block code without parsing its prose', async () => {
    stubDocument()
    const fetch = vi.fn(async () => jsonResponse({
      error: {
        code: 'preview_changed',
        message: 'Any localized or revised explanation can go here.',
      },
    }, 409))
    vi.stubGlobal('fetch', fetch)

    await expect(updatesApi.cleanupStorage(['sha256:old'])).rejects.toMatchObject({ status: 409, code: 'preview_changed' })
  })
})
