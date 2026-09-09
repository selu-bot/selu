import { describe, expect, it, vi } from 'vitest'
import { QueryClient } from '@tanstack/react-query'
import { createAppRouter, resolveAuthRedirect } from './app/router'
import { formatDayHeading, startHomeConversation } from './features/home/HomePage'
import { dateKeyInTimeZone, isSameDayInTimeZone, toDateTimeLocalInTimeZone } from './shared/dateTime'
import { setLanguage, translations } from './i18n'
import { appPath, normalizeBasePath } from './shared/paths'
import type { RetryableSend } from './shared/sendRetry'
import { prefersReducedMotion } from './shared/transitions'
import type { Conversation, Message } from './api'

const conversation: Conversation = {
  id: 'conversation-42', channel_id: 'personal', channel_name: 'Personal', title: null,
  status: 'active', kind: 'chat', created_at: '2026-09-07T10:00:00Z', last_activity_at: '2026-09-07T10:00:00Z', active_run_id: null,
}

describe('routing', () => {
  it.each([
    ['/app/', '/app/'],
    ['/app/login', '/app/login'],
    ['/app/setup', '/app/setup'],
    ['/app/conversations', '/app/conversations'],
    ['/app/conversations/abc', '/app/conversations/$conversationId'],
    ['/app/saved', '/app/saved'],
    ['/app/past', '/app/past'],
    ['/app/automations', '/app/automations'],
    ['/app/connections', '/app/connections'],
    ['/app/about-you', '/app/about-you'],
    ['/app/people', '/app/people'],
    ['/app/settings', '/app/settings'],
    ['/app/feedback', '/app/feedback'],
  ])('matches %s', (pathname, routeId) => {
    const router = createAppRouter(new QueryClient(), '')
    const matches = router.matchRoutes(pathname)
    expect(matches.at(-1)?.routeId).toBe(routeId)
  })

  it('redirects each auth state only where needed', () => {
    expect(resolveAuthRedirect('setup_required', '/app/')).toBe('/app/setup')
    expect(resolveAuthRedirect('setup_required', '/app/setup')).toBeNull()
    expect(resolveAuthRedirect('anonymous', '/app/conversations')).toBe('/app/login')
    expect(resolveAuthRedirect('anonymous', '/app/login')).toBeNull()
    expect(resolveAuthRedirect('authenticated', '/app/login')).toBe('/app')
    expect(resolveAuthRedirect('authenticated', '/app/conversations/abc')).toBeNull()
  })

  it('creates reverse-proxy-safe application URLs', () => {
    expect(normalizeBasePath('/selu/')).toBe('/selu')
    expect(normalizeBasePath('__SELU_BASE_PATH__')).toBe('')
    expect(appPath('/api/v1/auth/state', '/proxy/selu')).toBe('/proxy/selu/api/v1/auth/state')
  })
})

describe('Home conversation handoff', () => {
  it('shows the submitted message, navigates to the active conversation, and sends it immediately', async () => {
    const events: string[] = []
    let shown: Message | undefined
    await startHomeConversation({
      text: 'Help me prepare for tomorrow',
      create: async () => conversation,
      send: async (id, text, messageId) => { events.push(`send:${id}:${text}:${messageId}`) },
      showOptimistically: (_conversation, message) => { shown = message; events.push('optimistic') },
      navigate: async (id) => { events.push(`navigate:${id}`) },
      messageId: () => 'message-7',
    })
    expect(shown).toMatchObject({ id: 'message-7', role: 'user', content: 'Help me prepare for tomorrow' })
    expect(events).toEqual([
      'optimistic',
      'send:conversation-42:Help me prepare for tomorrow:message-7',
      'navigate:conversation-42',
    ])
  })

  it('starts and navigates a photo-only conversation', async () => {
    const events: string[] = []
    let shown: Message | undefined
    const attachments = [{ filename: 'garden.jpg', mime_type: 'image/jpeg', data_base64: '/9j/' }]
    const optimisticAttachments = [{ filename: 'garden.jpg', mime_type: 'image/jpeg', preview_url: 'data:image/jpeg;base64,/9j/', size_bytes: 3 }]
    await startHomeConversation({
      text: '',
      attachments,
      optimisticAttachments,
      create: async () => conversation,
      send: async (id, text, messageId, sentPhotos) => { events.push(`send:${id}:${text}:${messageId}:${sentPhotos?.[0]?.filename}`) },
      showOptimistically: (_conversation, message) => { shown = message; events.push(`optimistic:${message.content}`) },
      navigate: async (id) => { events.push(`navigate:${id}`) },
      messageId: () => 'message-photo',
    })
    expect(shown?.attachments).toEqual(optimisticAttachments)
    expect(events).toEqual([
      'optimistic:',
      'send:conversation-42::message-photo:garden.jpg',
      'navigate:conversation-42',
    ])
  })

  it('hands an ambiguous failure to Chat with the original message ID and photos', async () => {
    const events: string[] = []
    const failure = new TypeError('network interrupted')
    const selectedPhotos = [{
      id: 'photo-1', filename: 'retry.jpg', mime_type: 'image/jpeg', size_bytes: 3,
      data_base64: '/9j/', preview_url: 'data:image/jpeg;base64,/9j/',
    }]
    let retryDraft: RetryableSend | undefined
    await expect(startHomeConversation({
      text: 'Keep this draft',
      create: async () => conversation,
      send: async (_id, _text, messageId) => { events.push(`send:${messageId}`); throw failure },
      showOptimistically: () => { events.push('optimistic') },
      navigate: async () => { events.push('navigate') },
      onSendError: (failedConversation, messageId) => {
        retryDraft = { text: 'Keep this draft', messageId, photos: selectedPhotos }
        events.push(`retry:${failedConversation.id}:${messageId}`)
      },
      messageId: () => 'stable-message-id',
    })).rejects.toBe(failure)
    expect(events).toEqual([
      'optimistic',
      'send:stable-message-id',
      'navigate',
      'retry:conversation-42:stable-message-id',
    ])
    expect(retryDraft).toEqual({
      text: 'Keep this draft',
      messageId: 'stable-message-id',
      photos: selectedPhotos,
    })
  })
})

describe('Continuous Dayline timezone handling', () => {
  it('groups Today and Past by the authenticated timezone across midnight', () => {
    const reference = '2026-09-09T00:30:00Z'
    const activity = '2026-09-08T21:45:00Z'
    expect(dateKeyInTimeZone(reference, 'America/Los_Angeles')).toBe('2026-09-08')
    expect(dateKeyInTimeZone(reference, 'Europe/Berlin')).toBe('2026-09-09')
    expect(isSameDayInTimeZone(activity, reference, 'America/Los_Angeles')).toBe(true)
    expect(isSameDayInTimeZone(activity, reference, 'Europe/Berlin')).toBe(false)
  })

  it('formats spring-forward instants without inventing the skipped local hour', () => {
    expect(toDateTimeLocalInTimeZone('2026-03-29T00:30:00Z', 'Europe/Berlin')).toBe('2026-03-29T01:30')
    expect(toDateTimeLocalInTimeZone('2026-03-29T01:30:00Z', 'Europe/Berlin')).toBe('2026-03-29T03:30')
  })

  it('displays one-shot values in the selected automation timezone', () => {
    expect(toDateTimeLocalInTimeZone('2026-11-01T05:30:00Z', 'America/New_York')).toBe('2026-11-01T01:30')
    expect(toDateTimeLocalInTimeZone('2026-11-01T05:30:00Z', 'UTC')).toBe('2026-11-01T05:30')
  })

  it('formats day headings in Selu’s selected language and timezone', () => {
    const instant = '2026-09-08T22:30:00Z'
    try {
      setLanguage('de')
      expect(formatDayHeading(instant, 'Europe/Berlin')).toContain('Mittwoch')
      setLanguage('en')
      expect(formatDayHeading(instant, 'America/Los_Angeles')).toContain('Tuesday')
    } finally {
      setLanguage('en')
    }
  })
})

describe('localized and accessible behavior', () => {
  it('keeps English and German translation keys in parity', () => {
    expect(Object.keys(translations.de).sort()).toEqual(Object.keys(translations.en).sort())
  })

  it('detects reduced-motion preferences', () => {
    const match = vi.fn(() => ({ matches: true }) as MediaQueryList)
    expect(prefersReducedMotion(match)).toBe(true)
    expect(match).toHaveBeenCalledWith('(prefers-reduced-motion: reduce)')
  })
})
