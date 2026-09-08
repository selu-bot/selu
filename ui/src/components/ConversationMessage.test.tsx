import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Message } from '../api'
import { ConversationMessage } from './ConversationMessage'

beforeEach(() => {
  vi.stubGlobal('document', { querySelector: () => ({ getAttribute: () => '/selu' }) })
})

afterEach(() => vi.unstubAllGlobals())

function render(message: Partial<Message>) {
  return renderToStaticMarkup(createElement(ConversationMessage, {
    message: {
      id: 'message-1',
      role: 'user',
      content: '',
      created_at: '2026-09-08T12:00:00Z',
      compacted: false,
      ...message,
    },
  }))
}

describe('ConversationMessage photos', () => {
  it('renders a persisted photo-only message through the authenticated artifact route', () => {
    const html = render({
      attachments: [{ artifact_id: 'photo/id', filename: 'garden.jpg', mime_type: 'image/jpeg', size_bytes: 42 }],
    })

    expect(html).toContain('aria-label="Photos"')
    expect(html).toContain('href="/selu/api/v1/artifacts/photo%2Fid"')
    expect(html).toContain('src="/selu/api/v1/artifacts/photo%2Fid"')
    expect(html).toContain('aria-label="Open photo: garden.jpg"')
    expect(html).not.toContain('message-surface')
  })

  it('renders an optimistic photo preview without turning its data URL into a link', () => {
    const html = render({
      attachments: [{ filename: 'preview.png', mime_type: 'image/png', size_bytes: 3, preview_url: 'data:image/png;base64,AP8Q' }],
    })

    expect(html).toContain('src="data:image/png;base64,AP8Q"')
    expect(html).not.toContain('href="data:image/png')
    expect(html).not.toContain('message-surface')
  })

  it('hides engine-only attachment context after snapshot reconciliation', () => {
    const html = render({
      content: 'User sent image attachment(s) without accompanying text. Interpret what is visible and respond helpfully.\n\nAttached image artifacts:\n- artifact_id: photo/id | filename: garden.jpg | mime_type: image/jpeg | size_bytes: 42',
      attachments: [{ artifact_id: 'photo/id', filename: 'garden.jpg', mime_type: 'image/jpeg', size_bytes: 42 }],
    })

    expect(html).toContain('aria-label="Photos"')
    expect(html).not.toContain('User sent image attachment')
    expect(html).not.toContain('artifact_id')
    expect(html).not.toContain('message-surface')

    const captioned = render({
      content: 'Please read this\n\nAttached image artifacts:\n- artifact_id: photo/id',
      attachments: [{ artifact_id: 'photo/id', filename: 'garden.jpg', mime_type: 'image/jpeg', size_bytes: 42 }],
    })
    expect(captioned).toContain('Please read this')
    expect(captioned).not.toContain('artifact_id')
  })

  it('keeps text rendering and ignores non-photo or arbitrary preview sources', () => {
    const html = render({
      content: 'A safe caption',
      attachments: [
        { artifact_id: 'document', filename: 'notes.pdf', mime_type: 'application/pdf', size_bytes: 10 },
        { filename: 'remote.jpg', mime_type: 'image/jpeg', size_bytes: 10, preview_url: 'https://example.invalid/photo.jpg' },
      ],
    })

    expect(html).toContain('message-surface')
    expect(html).toContain('A safe caption')
    expect(html).not.toContain('example.invalid')
    expect(html).not.toContain('/api/v1/artifacts/document')
  })
})


describe('ConversationMessage tool activity', () => {
  it('shows metadata-free tool placeholders as friendly activity rows', () => {
    const html = render({
      role: 'assistant',
      content: '[calling web-browser__search]\n[calling delegate_to_agent]',
      tool_calls: undefined,
    })

    expect(html).toContain('role="list"')
    expect(html).toContain('Searched the web')
    expect(html).toContain('Brought in additional help')
    expect(html).not.toContain('[calling')
    expect(html).not.toContain('web-browser__search')
    expect(html).not.toContain('delegate_to_agent')
  })

  it('prefers structured tool names and safely labels unknown tools', () => {
    const html = render({
      role: 'assistant',
      content: '[calling persisted_placeholder]',
      tool_calls: [{ name: 'memory_search' }, { name: 'private_internal_tool' }],
    })

    expect(html).toContain('Checked saved information')
    expect(html).toContain('Completed another step')
    expect(html).not.toContain('persisted_placeholder')
    expect(html).not.toContain('private_internal_tool')
  })

  it('keeps ordinary assistant replies that merely mention a tool call', () => {
    const html = render({ role: 'assistant', content: 'I saw [calling support] in the notes.' })

    expect(html).toContain('message-surface')
    expect(html).toContain('I saw [calling support] in the notes.')
    expect(html).not.toContain('tool-activity-list')
  })
})


describe('ConversationMessage localized tool activity', () => {
  it('renders German activity labels', async () => {
    Object.assign(document, { documentElement: { lang: 'en' } })
    const { setLanguage } = await import('../i18n')
    setLanguage('de')
    try {
      const html = render({
        role: 'assistant',
        content: '[calling persisted_placeholder]',
        tool_calls: [{ name: 'web-browser__search' }, { name: 'delegate_to_agent' }],
      })

      expect(html).toContain('Im Web gesucht')
      expect(html).toContain('Weitere Unterstützung hinzugezogen')
      expect(html).not.toContain('web-browser__search')
      expect(html).not.toContain('delegate_to_agent')
    } finally {
      setLanguage('en')
    }
  })
})
