export type DateInput = string | number | Date

function asDate(value: DateInput) {
  return value instanceof Date ? value : new Date(value)
}

function formatter(locale: string, timeZone: string, options: Intl.DateTimeFormatOptions) {
  try {
    return new Intl.DateTimeFormat(locale, { ...options, timeZone })
  } catch {
    return new Intl.DateTimeFormat(locale, { ...options, timeZone: 'UTC' })
  }
}

function parts(value: DateInput, timeZone: string, withTime = false) {
  const date = asDate(value)
  if (Number.isNaN(date.valueOf())) return null
  const options: Intl.DateTimeFormatOptions = {
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    ...(withTime ? { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' as const } : {}),
  }
  const values = Object.fromEntries(
    formatter('en-US', timeZone, options)
      .formatToParts(date)
      .filter((part) => part.type !== 'literal')
      .map((part) => [part.type, part.value]),
  )
  return values as Record<string, string>
}

export function dateKeyInTimeZone(value: DateInput, timeZone: string) {
  const valueParts = parts(value, timeZone)
  return valueParts ? `${valueParts.year}-${valueParts.month}-${valueParts.day}` : ''
}

export function isSameDayInTimeZone(value: DateInput, reference: DateInput, timeZone: string) {
  const valueKey = dateKeyInTimeZone(value, timeZone)
  return Boolean(valueKey) && valueKey === dateKeyInTimeZone(reference, timeZone)
}

export function formatInTimeZone(
  value: DateInput,
  locale: string,
  timeZone: string,
  options: Intl.DateTimeFormatOptions,
) {
  const date = asDate(value)
  if (Number.isNaN(date.valueOf())) return ''
  return formatter(locale, timeZone, options).format(date)
}

export function toDateTimeLocalInTimeZone(value: DateInput, timeZone: string) {
  const valueParts = parts(value, timeZone, true)
  return valueParts
    ? `${valueParts.year}-${valueParts.month}-${valueParts.day}T${valueParts.hour}:${valueParts.minute}`
    : ''
}
