import { describe, expect, it } from 'vitest'
import { safeIssueUrl } from '../feedback/FeedbackPage'
import { formatBytes, isValidPublicOrigin } from '../settings/SettingsPage'
import { optimisticUpdateStatus } from '../updates/UpdatesPage'
import type { UpdateStatus } from '../updates/api'

describe('management safety and formatting helpers', () => {
  it('allows only HTTPS feedback links', () => {
    expect(safeIssueUrl('https://github.com/selu-bot/selu/issues/42')).toBe('https://github.com/selu-bot/selu/issues/42')
    expect(safeIssueUrl('http://example.test/issue')).toBeNull()
    expect(safeIssueUrl('javascript:alert(1)')).toBeNull()
    expect(safeIssueUrl('not a URL')).toBeNull()
  })

  it('formats cache sizes without exposing raw byte counts unnecessarily', () => {
    expect(formatBytes(512)).toBe('512 B')
    expect(formatBytes(1536)).toBe('1.5 KB')
    expect(formatBytes(10 * 1024 * 1024)).toBe('10 MB')
  })

  it('accepts only plain public HTTP addresses without credentials, paths, queries, or fragments', () => {
    expect(isValidPublicOrigin('')).toBe(true)
    expect(isValidPublicOrigin('https://selu.example.com')).toBe(true)
    expect(isValidPublicOrigin('http://localhost:8080')).toBe(true)
    expect(isValidPublicOrigin('https://user:secret@selu.example.com')).toBe(false)
    expect(isValidPublicOrigin('https://selu.example.com/app')).toBe(false)
    expect(isValidPublicOrigin('https://selu.example.com?token=secret')).toBe(false)
    expect(isValidPublicOrigin('not a URL')).toBe(false)
  })

  it('shows progress immediately after an update or rollback starts', () => {
    const idle = { active_job_id: '', status: 'idle', progress_key: '', last_error: 'old failure' } as UpdateStatus
    expect(optimisticUpdateStatus(idle, 'apply')).toMatchObject({
      active_job_id: 'starting-apply',
      status: 'updating',
      progress_key: 'updates.progress.preparing',
      last_error: '',
    })
    expect(optimisticUpdateStatus(idle, 'rollback')).toMatchObject({
      active_job_id: 'starting-rollback',
      status: 'updating',
      progress_key: 'updates.progress.rollback',
      last_error: '',
    })
  })
})
