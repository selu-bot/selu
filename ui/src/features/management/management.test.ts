import { describe, expect, it } from 'vitest'
import { safeIssueUrl } from '../feedback/FeedbackPage'
import { formatBytes } from '../settings/SettingsPage'

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
})
