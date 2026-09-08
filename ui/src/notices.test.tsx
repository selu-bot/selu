import { afterEach, describe, expect, it } from 'vitest'
import { ApiError } from './api'
import { setLanguage } from './i18n'
import { describeError } from './notices'

const ENGLISH_ERRORS = [
  ['conversation.invalid_photo', 'Choose a JPEG, PNG, GIF, or WebP image.'],
  ['conversation.photo_too_large', 'under 2 MB'],
  ['conversation.too_many_photos', 'up to 10 photos'],
  ['conversation.photo_command', 'can’t be sent with a command'],
] as const

afterEach(() => setLanguage('en'))

describe('photo error notices', () => {
  it.each(ENGLISH_ERRORS)('maps %s to plain-language English', (code, expected) => {
    expect(describeError(new ApiError(400, code), 'Photo not sent').body).toContain(expected)
  })

  it('maps local validation errors through the same allowlisted codes', () => {
    expect(describeError({ code: 'conversation.too_many_photos' }).body).toContain('up to 10 photos')
  })

  it('provides matching German guidance', () => {
    setLanguage('de')
    expect(describeError(new ApiError(400, 'conversation.invalid_photo')).body).toContain('JPEG-, PNG-, GIF- oder WebP-Bild')
    expect(describeError(new ApiError(400, 'conversation.photo_too_large')).body).toContain('höchstens 2 MB')
    expect(describeError(new ApiError(400, 'conversation.too_many_photos')).body).toContain('bis zu 10 Fotos')
    expect(describeError(new ApiError(400, 'conversation.photo_command')).body).toContain('nicht mit einem Befehl')
  })
})
