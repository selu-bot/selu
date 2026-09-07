import { describe, expect, it } from 'vitest'
import { cx, getNextEnabledIndex, mergeIds } from './utils'

describe('primitive helpers', () => {
  it('joins only present class names', () => {
    expect(cx('base', false, undefined, 'active', null)).toBe('base active')
  })

  it('merges ARIA id references without empty output', () => {
    expect(mergeIds('hint', undefined, 'error')).toBe('hint error')
    expect(mergeIds(undefined, undefined)).toBeUndefined()
  })

  it('wraps keyboard navigation and skips disabled items', () => {
    const disabled = [false, true, false, true]
    expect(getNextEnabledIndex(0, 1, disabled)).toBe(2)
    expect(getNextEnabledIndex(2, 1, disabled)).toBe(0)
    expect(getNextEnabledIndex(0, -1, disabled)).toBe(2)
  })

  it('returns no target when every item is disabled or absent', () => {
    expect(getNextEnabledIndex(0, 1, [true, true])).toBe(-1)
    expect(getNextEnabledIndex(0, 1, [])).toBe(-1)
  })
})
