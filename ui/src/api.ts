export type Conversation = {
  id: string
  channel_id: string
  channel_name: string
  title: string | null
  status: string
  kind: string
  created_at: string
  last_activity_at: string
  active_run_id: string | null
}

export type Message = {
  id: string
  role: 'user' | 'assistant' | 'tool' | 'system'
  content: string
  created_at: string
  compacted: boolean
  tool_calls?: unknown
  attachments?: unknown
}

export type ConversationPage = { conversations: Conversation[]; next_cursor?: string }
export const CONVERSATION_PAGE_SIZE = 40

export type Run = { id: string; client_message_id: string; status: string }
export type Session = { display_name: string; is_admin: boolean; language: string }
export type Approval = { approval_id: string; tool_name: string; message?: string; arguments?: unknown }
export type Snapshot = { conversation: Conversation; messages: Message[]; runs: Run[]; pending_approval: Approval | null; event_cursor: number }
export type ConversationEvent = {
  id: number
  conversation_id: string
  run_id?: string
  type: string
  payload: Record<string, unknown>
}

const basePath = document.querySelector('meta[name="selu-base-path"]')?.getAttribute('content') ?? ''
export const appPath = (path: string) => `${basePath}${path}`

export class ApiError extends Error {
  constructor(public readonly status: number, public readonly code?: string) {
    super(code ?? `Request failed (${status})`)
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(appPath(path), {
    credentials: 'same-origin',
    redirect: 'follow',
    ...init,
    headers: { Accept: 'application/json', ...init?.headers },
  })
  if (response.redirected && new URL(response.url).pathname.endsWith('/login')) {
    window.location.assign(response.url)
    throw new ApiError(401, 'session.expired')
  }
  if (!response.ok) {
    const detail = await response.json().catch(() => undefined) as { code?: string } | undefined
    throw new ApiError(response.status, detail?.code)
  }
  return response.status === 204 ? undefined as T : response.json() as Promise<T>
}

export const api = {
  session: () => request<Session>('/api/v1/session'),
  listConversations: (before?: string) => request<ConversationPage>(
    `/api/v1/conversations?limit=${CONVERSATION_PAGE_SIZE}${before ? `&before=${encodeURIComponent(before)}` : ''}`,
  ),
  renameConversation: (id: string, title: string) => request<Conversation>(`/api/v1/conversations/${id}`, {
    method: 'PATCH', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ title }),
  }),
  deleteConversation: (id: string) => request<void>(`/api/v1/conversations/${id}`, { method: 'DELETE' }),
  snapshot: (id: string) => request<Snapshot>(`/api/v1/conversations/${id}`),
  createConversation: () => request<Conversation>('/api/v1/conversations', {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({}),
  }),
  send: (id: string, text: string, clientMessageId: string) => request<{ run: Run }>(
    `/api/v1/conversations/${id}/messages`,
    { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ text, client_message_id: clientMessageId }) },
  ),
  decideApproval: (id: string, approved: boolean) => request<void>(`/api/v1/approvals/${id}/decision`, {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ approved }),
  }),
  events: openEvents,
}

export function openEvents(after: number, onEvent: (event: ConversationEvent) => void, onGap: () => void): EventSource {
  const source = new EventSource(appPath(`/api/v1/events?after=${after}`), { withCredentials: true })
  source.onmessage = () => undefined
  source.onerror = () => onGap()
  for (const type of ['run.created', 'run.updated', 'run.progress', 'message.created', 'message.text_delta', 'message.part_finished', 'message.artifacts', 'approval.requested', 'run.output_finished', 'run.error', 'conversation.changed', 'conversation.deleted']) {
    source.addEventListener(type, (raw) => onEvent(JSON.parse((raw as MessageEvent).data) as ConversationEvent))
  }
  return source
}
