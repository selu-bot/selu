import { apiRequest } from '../../api'

export type ConnectorKind = 'web' | 'webhook' | 'telegram' | 'imessage' | 'whatsapp'
export type ConnectorPerson = { ref_id: string; user_id: string; display_name: string; username: string; sender_ref: string }
export type Connector = {
  pipe_id: string
  config_id: string | null
  kind: ConnectorKind
  name: string
  owner_user_id: string
  owner_name: string
  active: boolean
  callback_url: string | null
  server_url: string | null
  chat_ref: string | null
  people: ConnectorPerson[]
  created_at: string
}
export type ConnectorUser = { id: string; display_name: string; username: string }
export type ConnectorsSnapshot = { connectors: Connector[]; users: ConnectorUser[]; telegram_https_ready: boolean }
export type PersonInput = { user_id: string; sender_ref: string }
export type WebhookReceipt = { pipe_id: string; inbound_url?: string; inbound_token?: string }
export type TelegramChat = { chat_id: string; display_name: string; chat_type: string; last_message: string }
export type ImessageChat = { guid: string; display_name: string; participants: string[]; is_group: boolean; last_message: string }
export type ChatSearch<T> = { ok: boolean; error?: string; chats?: T[]; bot_username?: string }
export type WhatsappChat = { sender_ref: string; label: string }
export type WhatsappChats = { ok: boolean; running: boolean; connection_state?: string; message?: string; chats: WhatsappChat[] }
export type WhatsappStatus = { running: boolean; connection_state?: string; requires_qr: boolean; qr_data_url?: string; jid?: string; last_error?: string; message?: string }
export type TelegramWebhook = { registered: boolean; url: string; pending_updates: number; last_error?: string }

const json = (method: string, body?: unknown): RequestInit => ({
  method,
  headers: body === undefined ? undefined : { 'Content-Type': 'application/json' },
  body: body === undefined ? undefined : JSON.stringify(body),
})

export const connectorsApi = {
  list: () => apiRequest<ConnectorsSnapshot>('/api/v1/connectors'),
  createSimple: (input: { kind: 'web' | 'webhook'; owner_user_id: string; name: string; callback_url?: string; callback_authorization?: string }) => apiRequest<WebhookReceipt>('/api/v1/connectors', json('POST', input)),
  createImessage: (input: { name: string; server_url: string; server_password: string; chat_guid: string; people: PersonInput[]; callback_base_url?: string }) => apiRequest<{ ok: boolean }>('/api/v1/connectors/imessage', json('POST', input)),
  imessageChats: (server_url: string, server_password: string) => apiRequest<ChatSearch<ImessageChat>>('/api/v1/connectors/imessage/chats', json('POST', { server_url, server_password })),
  createTelegram: (input: { name: string; bot_token: string; chat_id: string; people: PersonInput[] }) => apiRequest<{ ok: boolean }>('/api/v1/connectors/telegram', json('POST', input)),
  telegramChats: (bot_token: string) => apiRequest<ChatSearch<TelegramChat>>('/api/v1/connectors/telegram/chats', json('POST', { bot_token })),
  createWhatsapp: (input: { name: string; callback_authorization?: string; people: PersonInput[] }) => apiRequest<{ ok: boolean }>('/api/v1/connectors/whatsapp', json('POST', input)),
  whatsappChats: (query = '') => apiRequest<WhatsappChats>(`/api/v1/connectors/whatsapp/chats?q=${encodeURIComponent(query)}`),
  whatsappStatus: () => apiRequest<WhatsappStatus>('/api/v1/connectors/whatsapp/status'),
  addPerson: (pipeId: string, input: PersonInput) => apiRequest<void>(`/api/v1/connectors/${encodeURIComponent(pipeId)}/people`, json('POST', input)),
  removePerson: (pipeId: string, refId: string) => apiRequest<void>(`/api/v1/connectors/${encodeURIComponent(pipeId)}/people/${encodeURIComponent(refId)}`, { method: 'DELETE' }),
  telegramWebhook: (pipeId: string) => apiRequest<TelegramWebhook>(`/api/v1/connectors/${encodeURIComponent(pipeId)}/telegram/webhook`),
  refreshTelegramWebhook: (pipeId: string) => apiRequest<{ ok: boolean }>(`/api/v1/connectors/${encodeURIComponent(pipeId)}/telegram/webhook`, json('POST')),
  remove: (pipeId: string) => apiRequest<void>(`/api/v1/connectors/${encodeURIComponent(pipeId)}`, { method: 'DELETE' }),
}
