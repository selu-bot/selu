import { createClientId } from './clientId'
import type { SelectedPhoto } from './photoUploads'

export type RetryableSend = {
  text: string
  messageId: string
  photos: SelectedPhoto[]
}

export function failedSendQueryKey(conversationId: string) {
  return ['failed-send', conversationId] as const
}

export function messageIdForSend(retryMessageId: string | null, createId: () => string = createClientId) {
  return retryMessageId ?? createId()
}
