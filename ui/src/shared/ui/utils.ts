export function cx(...values: Array<string | false | null | undefined>): string {
  return values.filter(Boolean).join(' ')
}

export function mergeIds(...ids: Array<string | undefined>): string | undefined {
  const value = ids.filter(Boolean).join(' ')
  return value || undefined
}

export function getNextEnabledIndex(
  current: number,
  direction: 1 | -1,
  disabled: readonly boolean[],
): number {
  if (disabled.length === 0 || disabled.every(Boolean)) return -1
  let candidate = current
  for (let step = 0; step < disabled.length; step += 1) {
    candidate = (candidate + direction + disabled.length) % disabled.length
    if (!disabled[candidate]) return candidate
  }
  return -1
}

export const focusableSelector = [
  'a[href]',
  'button:not([disabled])',
  'input:not([disabled]):not([type="hidden"])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  '[tabindex]:not([tabindex="-1"])',
].join(',')
