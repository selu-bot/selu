import { describe, expect, it, vi } from 'vitest'
import { failedSendQueryKey, messageIdForSend } from './sendRetry'

describe('safe message retries', () => {
  it('reuses the original logical message ID without generating another', () => {
    const create = vi.fn(() => 'new-id')
    expect(messageIdForSend('accepted-id', create)).toBe('accepted-id')
    expect(create).not.toHaveBeenCalled()
  })

  it('generates an ID only for a new logical message', () => {
    expect(messageIdForSend(null, () => 'new-id')).toBe('new-id')
    expect(failedSendQueryKey('conversation-1')).toEqual(['failed-send', 'conversation-1'])
  })
})
