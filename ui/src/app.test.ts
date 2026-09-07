import { describe, expect, it, vi } from 'vitest'
import { QueryClient } from '@tanstack/react-query'
import { createAppRouter, resolveAuthRedirect } from './app/router'
import { startHomeConversation } from './features/home/HomePage'
import { translations } from './i18n'
import { appPath, normalizeBasePath } from './shared/paths'
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
