import { describe, expect, it } from 'vitest'
import type { Conversation } from '../../api'
import { canSaveTopic, completedTodayConversations, shouldLoadNextHomePage } from './HomePage'
import { pastConversationsBeforeToday } from './ConversationArchivePage'

const conversation: Conversation = {
  id: 'conversation',
  channel_id: 'web',
  channel_name: 'Web',
  title: 'Result',
  status: 'open',
  kind: 'conversation',
  created_at: '2026-09-09T08:00:00.000Z',
  last_activity_at: '2026-09-09T08:00:00.000Z',
  active_run_id: null,
}

describe('Continuous Dayline save eligibility', () => {
  it('never allows an automation-owned scheduled result to be saved', () => {
    expect(canSaveTopic({ ...conversation, kind: 'schedule', can_save: true })).toBe(false)
    expect(canSaveTopic({ ...conversation, kind: 'schedule', can_save: undefined })).toBe(false)
  })

  it('preserves the API save guard for ordinary conversations', () => {
    expect(canSaveTopic(conversation)).toBe(true)
    expect(canSaveTopic({ ...conversation, can_save: false })).toBe(false)
  })
})


describe('Stage 3 recent-first timeline behavior', () => {
  const withActivity = (id: string, lastActivityAt: string, activeRunId: string | null = null): Conversation => ({
    ...conversation,
    id,
    created_at: lastActivityAt,
    last_activity_at: lastActivityAt,
    active_run_id: activeRunId,
  })

  it('keeps completed Today entries in newest-first DOM order and excludes future or active work', () => {
    const reference = '2026-09-09T12:00:00Z'
    const result = completedTodayConversations([
      withActivity('older', '2026-09-09T08:00:00Z'),
      withActivity('future', '2026-09-09T13:00:00Z'),
      withActivity('newest', '2026-09-09T11:30:00Z'),
      withActivity('active', '2026-09-09T10:00:00Z', 'run-1'),
      withActivity('past', '2026-09-08T23:59:59Z'),
    ], reference, 'UTC')

    expect(result.map((item) => item.id)).toEqual(['newest', 'older'])
  })

  it('keeps Past Days strictly before the authenticated local today', () => {
    const reference = '2026-09-09T00:30:00Z'
    const items = [
      withActivity('previous-local-day', '2026-09-08T06:30:00Z'),
      withActivity('same-local-day', '2026-09-08T21:45:00Z'),
      withActivity('future-local-day', '2026-09-09T10:00:00Z'),
    ]

    expect(pastConversationsBeforeToday(items, reference, 'America/Los_Angeles').map((item) => item.id))
      .toEqual(['previous-local-day'])
    expect(pastConversationsBeforeToday(items, reference, 'Europe/Berlin').map((item) => item.id))
      .toEqual(['previous-local-day', 'same-local-day'])
  })

  it('automatically follows pages until every possible same-day entry is loaded', () => {
    const reference = '2026-09-09T12:00:00Z'
    const firstForty = Array.from({ length: 40 }, (_, index) =>
      withActivity(`today-${index}`, '2026-09-09T10:00:00Z'))
    const firstPage = { conversations: firstForty, next_cursor: 'cursor-1' }
    const secondPage = { conversations: [withActivity('today-41', '2026-09-09T08:00:00Z')], next_cursor: 'cursor-2' }
    const boundaryPage = { conversations: [withActivity('yesterday', '2026-09-08T23:59:59Z')], next_cursor: 'cursor-3' }

    expect(shouldLoadNextHomePage([firstPage], reference, 'UTC')).toBe(true)
    expect(shouldLoadNextHomePage([firstPage, secondPage], reference, 'UTC')).toBe(true)
    expect(shouldLoadNextHomePage([firstPage, secondPage, boundaryPage], reference, 'UTC')).toBe(false)
    expect(shouldLoadNextHomePage([firstPage, { ...secondPage, next_cursor: 'cursor-1' }], reference, 'UTC')).toBe(false)
  })
})
