import { describe, expect, it, vi } from 'vitest'
import { createClientId, type ClientIdSource } from './clientId'

describe('createClientId', () => {
  it('uses the browser UUID implementation when available', () => {
    const randomUUID = vi.fn(() => '3c758f0e-8a14-4c99-a74d-5b11cb4144f1')
    const source: ClientIdSource = { randomUUID, getRandomValues: vi.fn() }

    expect(createClientId(source)).toBe('3c758f0e-8a14-4c99-a74d-5b11cb4144f1')
    expect(randomUUID).toHaveBeenCalledOnce()
    expect(source.getRandomValues).not.toHaveBeenCalled()
  })

  it('creates an RFC 4122 version 4 UUID when randomUUID is unavailable', () => {
    const source: ClientIdSource = {
      getRandomValues: (values) => {
        values.set([0, 1, 2, 3, 4, 5, 0xff, 7, 0xff, 9, 10, 11, 12, 13, 14, 15])
        return values
      },
    }

    expect(createClientId(source)).toBe('00010203-0405-4f07-bf09-0a0b0c0d0e0f')
  })
})
