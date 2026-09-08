import { afterEach, describe, expect, it, vi } from 'vitest'
import { api, type PhotoUpload } from './api'

const runResponse = { run: { id: 'run-1', client_message_id: 'message-1', status: 'queued' } }

afterEach(() => vi.unstubAllGlobals())

function mockFetch() {
  const fetch = vi.fn(async (_input: RequestInfo | URL, _init?: RequestInit) => new Response(JSON.stringify(runResponse), {
    status: 200,
    headers: { 'Content-Type': 'application/json' },
  }))
  vi.stubGlobal('document', { querySelector: () => null })
  vi.stubGlobal('fetch', fetch)
  return fetch
}

describe('conversation message API', () => {
  it('keeps the text-only request body unchanged', async () => {
    const fetch = mockFetch()
    await api.send('conversation-1', 'Hello', 'message-1')

    const init = fetch.mock.calls[0]?.[1]
    expect(JSON.parse(String(init?.body))).toEqual({ text: 'Hello', client_message_id: 'message-1' })
  })

  it('serializes selected photos in the v1 attachment contract', async () => {
    const fetch = mockFetch()
    const attachments: PhotoUpload[] = [{ filename: 'garden.jpg', mime_type: 'image/jpeg', data_base64: '/9j/' }]
    await api.send('conversation-1', '', 'message-1', attachments)

    const init = fetch.mock.calls[0]?.[1]
    expect(JSON.parse(String(init?.body))).toEqual({ text: '', client_message_id: 'message-1', attachments })
  })
})
