export type Conversation = {
  id: string
  channel_id: string
  channel_name: string
  title: string | null
  status: string
  kind: string
  schedule_id?: string | null
  created_at: string
  last_activity_at: string
  active_run_id: string | null
  saved_at?: string | null
  can_save?: boolean
  preview?: string | null
  message_count?: number
}

export type MessageAttachment = {
  artifact_id?: string
  filename: string
  mime_type: string
  size_bytes: number
  preview_url?: string
}

export type Message = {
  id: string
  role: 'user' | 'assistant' | 'tool' | 'system'
  content: string
  created_at: string
  compacted: boolean
  tool_calls?: unknown
  attachments?: MessageAttachment[] | null
}

export type ConversationPage = { conversations: Conversation[]; next_cursor?: string }
export const CONVERSATION_PAGE_SIZE = 40

export type Run = { id: string; client_message_id: string; status: string; created_at: string; started_at: string | null; completed_at: string | null }
export type Session = { display_name: string; is_admin: boolean; language: string; timezone: string; supports_photo_uploads?: boolean }
export type PhotoUpload = { filename: string; mime_type: string; data_base64: string }
export type AuthUser = { user_id?: string; display_name?: string; username?: string; is_admin?: boolean; language?: string }
export type AuthState = { status: 'setup_required' | 'anonymous' | 'authenticated'; user?: AuthUser }
export type LoginInput = { username: string; password: string }
export type SetupInput = { display_name: string; username: string; password: string; language: 'en' | 'de' }
export type Approval = { approval_id: string; tool_name: string; message?: string; arguments?: unknown }
export type TurnRating = 1 | -1
export type SlashCommand = { command: string; label: string; description: string; argument_hint: string | null }
export type Snapshot = { conversation: Conversation; messages: Message[]; runs: Run[]; pending_approval: Approval | null; latest_turn_rating: number | null; event_cursor: number }
export type ConversationEvent = {
  id: number
  conversation_id: string
  run_id?: string
  type: string
  payload: Record<string, unknown>
}

import { appPath } from './shared/paths'

export { appPath } from './shared/paths'

export class ApiError extends Error {
  constructor(public readonly status: number, public readonly code?: string) {
    super(code ?? `Request failed (${status})`)
  }
}

export async function apiRequest<T>(path: string, init?: RequestInit): Promise<T> {
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
    const detail = await response.json().catch(() => undefined) as { code?: string; error?: { code?: string } } | undefined
    throw new ApiError(response.status, detail?.code ?? detail?.error?.code)
  }
  return response.status === 204 ? undefined as T : response.json() as Promise<T>
}

export const api = {
  authState: () => apiRequest<AuthState>('/api/v1/auth/state'),
  login: (input: LoginInput) => apiRequest<AuthState>('/api/v1/auth/login', {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(input),
  }),
  setup: (input: SetupInput) => apiRequest<AuthState>('/api/v1/auth/setup', {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(input),
  }),
  logout: () => apiRequest<void>('/api/v1/auth/logout', { method: 'POST' }),
  session: () => apiRequest<Session>('/api/v1/session'),
  listConversations: (before?: string, saved?: boolean) => {
    const params = new URLSearchParams({ limit: String(CONVERSATION_PAGE_SIZE) })
    if (before) params.set('before', before)
    if (saved !== undefined) params.set('saved', String(saved))
    return apiRequest<ConversationPage>(`/api/v1/conversations?${params}`)
  },
  updateConversation: (id: string, input: { title?: string; saved?: boolean }) => apiRequest<Conversation>(`/api/v1/conversations/${id}`, {
    method: 'PATCH', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(input),
  }),
  renameConversation: (id: string, title: string) => apiRequest<Conversation>(`/api/v1/conversations/${id}`, {
    method: 'PATCH', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ title }),
  }),
  setConversationSaved: (id: string, saved: boolean, title?: string) => apiRequest<Conversation>(`/api/v1/conversations/${id}`, {
    method: 'PATCH', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ saved, ...(title ? { title } : {}) }),
  }),
  deleteConversation: (id: string) => apiRequest<void>(`/api/v1/conversations/${id}`, { method: 'DELETE' }),
  snapshot: (id: string) => apiRequest<Snapshot>(`/api/v1/conversations/${id}`),
  createConversation: () => apiRequest<Conversation>('/api/v1/conversations', {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({}),
  }),
  send: (id: string, text: string, clientMessageId: string, attachments: PhotoUpload[] = []) => apiRequest<{ run: Run }>(
    `/api/v1/conversations/${id}/messages`,
    { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ text, client_message_id: clientMessageId, ...(attachments.length ? { attachments } : {}) }) },
  ),
  decideApproval: (id: string, approved: boolean) => apiRequest<void>(`/api/v1/approvals/${id}/decision`, {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ approved }),
  }),
  commands: (lang: string) => apiRequest<{ commands: SlashCommand[] }>(`/api/v1/commands?lang=${encodeURIComponent(lang)}`),
  rateLatestTurn: (id: string, rating: TurnRating) => apiRequest<void>(`/api/v1/conversations/${id}/feedback`, {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ rating }),
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
