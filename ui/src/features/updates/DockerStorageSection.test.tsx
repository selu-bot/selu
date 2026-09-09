import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it, vi } from 'vitest'
import { ApiError } from '../../api'
import type { DockerStorageStatus } from './api'
import {
  DockerStorageSummary,
  StoragePreviewContent,
  cleanupCandidateImageIds,
  getStorageActivity,
  storageCleanupErrorBody,
  storageCleanupNotice,
  storageMessages,
  storageSnapshotKey,
  storageUiReducer,
  type StorageUiState,
} from './DockerStorageSection'

const gib = 1024 ** 3
const mib = 1024 ** 2

const storage: DockerStorageStatus = {
  managed_bytes: 3 * gib,
  protected_bytes: 2.5 * gib,
  reclaimable_bytes: 512 * mib,
  managed_image_count: 3,
  protected_image_count: 2,
  reclaimable_image_count: 1,
  last_cleanup_at: '2026-09-08T10:00:00Z',
  blocked_code: '',
  blocked_reason: '',
  entries: [
    { image_id: 'sha256:agent', display_name: 'Calendar agent', size_bytes: 1.5 * gib, state: 'protected', reason: 'current_revision' },
    { image_id: 'sha256:rollback', display_name: 'Calendar agent · previous', size_bytes: gib, state: 'protected', reason: 'previous_revision' },
    { image_id: 'sha256:old', display_name: 'Unused image', size_bytes: 512 * mib, state: 'reclaimable', reason: 'unreferenced' },
  ],
}

const initialUi: StorageUiState = {
  previewOpen: false,
  previewSnapshotKey: null,
  previewImageIds: [],
  confirmCleanup: false,
  confirmationSnapshotKey: null,
  confirmedImageIds: [],
}

function renderSummary(overrides: Partial<DockerStorageStatus> = {}, systemUpdateActive = false, cleanupActive = false) {
  return renderToStaticMarkup(createElement(DockerStorageSummary, {
    storage: { ...storage, ...overrides },
    copy: storageMessages.en,
    language: 'en',
    systemUpdateActive,
    cleanupActive,
    onPreview: vi.fn(),
  }))
}

function buttonTag(html: string, label: string) {
  const labelIndex = html.indexOf(label)
  const start = html.lastIndexOf('<button', labelIndex)
  return html.slice(start, html.indexOf('>', start) + 1)
}

describe('Docker storage summary', () => {
  it('renders consistent managed and safely removable totals with preview-first actions', () => {
    const html = renderSummary()

    expect(storage.entries.reduce((total, entry) => total + entry.size_bytes, 0)).toBe(storage.managed_bytes)
    expect(storage.protected_bytes + storage.reclaimable_bytes).toBe(storage.managed_bytes)
    expect(storageMessages.en.title).toBe('Docker storage')
    expect(html).toContain('Managed total')
    expect(html).toContain('3 GB')
    expect(html).toContain('Safely removable')
    expect(html).toContain('512 MB')
    expect(html).toContain('Safe cleanup available')
    expect(html).toContain('Preview cleanup')
    expect(html).toContain('Clean up 512 MB')
    expect(buttonTag(html, 'Clean up 512 MB')).not.toContain('disabled')
  })

  const blockedCases: Array<[string, Partial<DockerStorageStatus>, boolean, boolean, string]> = [
    ['a system update', {}, true, false, 'Update in progress'],
    ['another cleanup', {}, false, true, 'Cleanup in progress'],
    ['a backend Docker block', { blocked_code: 'docker_unavailable', blocked_reason: 'Explanation text can change.' }, false, false, 'Cleanup paused'],
    ['unavailable updater metadata', { blocked_code: 'safety_metadata_unavailable', blocked_reason: 'Explanation text can change.' }, false, false, 'Cleanup paused'],
    ['an update code reported by the backend', { blocked_code: 'update_active', blocked_reason: 'Explanation text can change.' }, false, false, 'Update in progress'],
    ['nothing reclaimable', { reclaimable_bytes: 0, reclaimable_image_count: 0 }, false, false, 'Everything is protected'],
  ]

  it.each(blockedCases)('disables cleanup during %s', (_name, overrides, updateActive, cleanupActive, statusLabel) => {
    const html = renderSummary(overrides, updateActive, cleanupActive)

    expect(getStorageActivity({ ...storage, ...overrides }, updateActive, cleanupActive)).not.toBe('ready')
    expect(html).toContain(statusLabel)
    expect(buttonTag(html, overrides.reclaimable_bytes === 0 ? 'Clean up 0 B' : 'Clean up 512 MB')).toContain('disabled=""')
  })

  it('does not infer update activity from English reason prose', () => {
    const snapshot = {
      ...storage,
      blocked_code: 'safety_metadata_unavailable' as const,
      blocked_reason: 'Storage cleanup is paused while a Selu update is active.',
    }
    expect(getStorageActivity(snapshot, false, false)).toBe('blocked')
  })

  it('keeps the read-only preview available during an update', () => {
    const html = renderSummary({}, true)
    expect(buttonTag(html, 'Preview cleanup')).not.toContain('disabled')
  })
})

describe('Docker storage preview', () => {
  it('lists every entry with labels selected from structured reason codes plus all totals', () => {
    const html = renderToStaticMarkup(createElement(StoragePreviewContent, { storage, copy: storageMessages.en, language: 'en' }))

    expect(html).toContain('Storage totals')
    expect(html).toContain('Protected')
    expect(html).toContain('Calendar agent')
    expect(html).toContain('Needed by an installed agent')
    expect(html).toContain('Saved for a rollback version')
    expect(html).toContain('Safe to remove')
    expect(html).toContain('No longer needed by an installed agent or rollback version')
  })

  it('explains a structured blocked state inside the preview sheet content', () => {
    const blocked = { ...storage, blocked_code: 'docker_unavailable' as const, blocked_reason: 'Arbitrary prose.' }
    const html = renderToStaticMarkup(createElement(StoragePreviewContent, {
      storage: blocked,
      copy: storageMessages.en,
      language: 'en',
      activity: 'blocked',
    }))

    expect(html).toContain('Cleanup is not ready yet')
    expect(html).toContain('Cleanup paused')
    expect(html).toContain('Docker storage is temporarily unavailable')
    expect(html).toContain('role="status"')
  })

  it('provides matching German labels and complete cleanup notices', () => {
    const html = renderToStaticMarkup(createElement(StoragePreviewContent, { storage, copy: storageMessages.de, language: 'de' }))
    const notice = storageCleanupNotice(storageMessages.de, {
      ...storage,
      reclaimed_bytes: 512 * mib,
      reclaimed_image_count: 1,
    }, 'de')

    expect(html).toContain('Speicherübersicht')
    expect(html).toContain('Wird von einem installierten Agenten benötigt')
    expect(html).toContain('Sicher entfernbar')
    expect(notice).toEqual({
      title: 'Docker-Speicher bereinigt',
      body: 'Selu hat 1 Image sicher entfernt und 512 MB freigegeben. Geschützte Images blieben unangetastet.',
    })
  })

  it('uses honest localized partial-success copy when some candidates remain', () => {
    const result = {
      ...storage,
      blocked_code: 'cleanup_incomplete' as const,
      reclaimed_bytes: 512 * mib,
      reclaimed_image_count: 1,
    }

    expect(storageCleanupNotice(storageMessages.en, result, 'en')).toEqual({
      title: 'Some Docker storage was cleaned up',
      body: 'Selu safely removed 1 image and freed 512 MB. Some images could not be removed safely, so Selu left them in place.',
    })
    expect(storageCleanupNotice(storageMessages.de, result, 'de')).toEqual({
      title: 'Ein Teil des Docker-Speichers wurde bereinigt',
      body: 'Selu hat 1 Image sicher entfernt und 512 MB freigegeben. Einige Images konnten nicht sicher entfernt werden und blieben deshalb erhalten.',
    })
  })

  it('maps structured cleanup conflicts to actionable English and German guidance', () => {
    expect(storageCleanupErrorBody(storageMessages.en, new ApiError(409, 'preview_changed')))
      .toContain('Open the preview')
    expect(storageCleanupErrorBody(storageMessages.de, new ApiError(409, 'update_active')))
      .toContain('Systemaktualisierung')
    expect(storageCleanupErrorBody(storageMessages.en, new ApiError(409, 'docker_unavailable')))
      .toContain('Check Docker')
    expect(storageCleanupErrorBody(storageMessages.de, new Error('network')))
      .toContain('Speicherstatus')
  })

  it('describes current system images separately from installed agent images', () => {
    const systemStorage: DockerStorageStatus = {
      ...storage,
      entries: [{
        image_id: 'sha256:selu',
        display_name: 'Selu',
        size_bytes: gib,
        state: 'protected',
        reason: 'current_system_image',
      }],
    }
    const html = renderToStaticMarkup(createElement(StoragePreviewContent, {
      storage: systemStorage,
      copy: storageMessages.en,
      language: 'en',
    }))
    expect(html).toContain('Needed by Selu right now')
    expect(html).not.toContain('Needed by an installed agent')
  })
})

describe('Docker storage preview and cleanup transitions', () => {
  it('requires preview before confirmation and carries the exact reviewed image IDs', () => {
    const key = storageSnapshotKey(storage)
    expect(storageUiReducer(initialUi, { type: 'request-cleanup' })).toEqual(initialUi)

    const preview = storageUiReducer(initialUi, {
      type: 'open-preview',
      snapshotKey: key,
      imageIds: cleanupCandidateImageIds(storage),
    })
    const confirming = storageUiReducer(preview, { type: 'request-cleanup' })

    expect(preview.previewOpen).toBe(true)
    expect(confirming).toEqual({
      ...initialUi,
      confirmCleanup: true,
      confirmationSnapshotKey: key,
      confirmedImageIds: ['sha256:old'],
    })
    expect(storageUiReducer(confirming, { type: 'cleanup-success' })).toEqual(initialUi)
  })

  it('clears confirmation intent when readiness or any reviewed snapshot field changes', () => {
    const key = storageSnapshotKey(storage)
    const preview = storageUiReducer(initialUi, {
      type: 'open-preview',
      snapshotKey: key,
      imageIds: ['sha256:old'],
    })
    const confirming = storageUiReducer(preview, { type: 'request-cleanup' })

    expect(storageUiReducer(confirming, { type: 'snapshot-changed', snapshotKey: key, ready: true })).toEqual(confirming)
    expect(storageUiReducer(confirming, { type: 'snapshot-changed', snapshotKey: `${key}-changed`, ready: true })).toEqual(initialUi)
    expect(storageUiReducer(confirming, { type: 'snapshot-changed', snapshotKey: key, ready: false })).toEqual(initialUi)
    expect(storageUiReducer(confirming, { type: 'cleanup-error' })).toEqual(initialUi)
  })

  it('invalidates an open preview when its candidate snapshot changes', () => {
    const key = storageSnapshotKey(storage)
    const preview = storageUiReducer(initialUi, {
      type: 'open-preview',
      snapshotKey: key,
      imageIds: ['sha256:old'],
    })

    expect(storageUiReducer(preview, { type: 'snapshot-changed', snapshotKey: `${key}-changed`, ready: true })).toEqual(initialUi)
  })
})
