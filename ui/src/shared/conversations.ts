import type { Conversation, ConversationPage } from '../api'

export type ConversationPages = { pages: ConversationPage[]; pageParams: unknown[] }

export function dedupeConversations(items: Conversation[]) {
  const seen = new Set<string>()
  return items.filter((item) => {
    if (seen.has(item.id)) return false
    seen.add(item.id)
    return true
  })
}

export function prependConversation(old: ConversationPages | undefined, conversation: Conversation): ConversationPages {
  if (!old || old.pages.length === 0) return { pages: [{ conversations: [conversation] }], pageParams: [''] }
  const [first, ...rest] = old.pages
  return {
    ...old,
    pages: [{ ...first, conversations: [conversation, ...first.conversations.filter((item) => item.id !== conversation.id)] }, ...rest],
  }
}

export function replaceConversation(old: ConversationPages | undefined, conversation: Conversation): ConversationPages | undefined {
  if (!old) return old
  return {
    ...old,
    pages: old.pages.map((page) => ({
      ...page,
      conversations: page.conversations.map((item) => item.id === conversation.id ? conversation : item),
    })),
  }
}

export function removeConversation(old: ConversationPages | undefined, id: string): ConversationPages | undefined {
  if (!old) return old
  return {
    ...old,
    pages: old.pages.map((page) => ({ ...page, conversations: page.conversations.filter((item) => item.id !== id) })),
  }
}
