import { describe, expect, it } from 'vitest'
import type { Conversation } from '../../api'
import { canSaveTopic } from './HomePage'

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
