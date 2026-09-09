import { useMemo, useState } from 'react'
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Bookmark, CalendarDays, Search } from 'lucide-react'
import { api, type Conversation } from '../../api'
import { SaveTopicDialog } from '../../components/ConversationActions'
import { t, useLanguage } from '../../i18n'
import { useNotices, useQueryErrorNotice } from '../../notices'
import { dedupeConversations, replaceConversation, type ConversationPages } from '../../shared/conversations'
import { dateKeyInTimeZone, isSameDayInTimeZone } from '../../shared/dateTime'
import { AppPageShell } from '../shell/AppPageShell'
import { formatDayHeading, TimelineEntry } from './HomePage'

export function SavedTopicsPage() {
  return <ConversationArchivePage mode="saved" />
}

export function PastDaysPage() {
  return <ConversationArchivePage mode="past" />
}

function ConversationArchivePage({ mode }: { mode: 'saved' | 'past' }) {
  useLanguage()
  const cache = useQueryClient()
  const notices = useNotices()
  const [query, setQuery] = useState('')
  const [saveTarget, setSaveTarget] = useState<Conversation | null>(null)
  const queryKey = ['conversation-archive', mode] as const
  const session = useQuery({ queryKey: ['session'], queryFn: api.session, staleTime: Infinity })
  const timezone = session.data?.timezone ?? 'UTC'
  const conversations = useInfiniteQuery({
    queryKey,
    queryFn: ({ pageParam }) => api.listConversations(pageParam || undefined, mode === 'saved' ? true : undefined),
    initialPageParam: '',
    getNextPageParam: (lastPage) => lastPage.next_cursor ?? undefined,
  })
  useQueryErrorNotice(session.error ?? conversations.error)
  const items = useMemo(() => {
    const all = dedupeConversations(conversations.data?.pages.flatMap((page) => page.conversations) ?? [])
    const scoped = mode === 'past' ? all.filter((item) => !isSameDayInTimeZone(item.last_activity_at, Date.now(), timezone)) : all
    const term = query.trim().toLocaleLowerCase()
    return scoped.filter((item) => !term || [item.title, item.preview, item.channel_name].some((value) => value?.toLocaleLowerCase().includes(term)))
  }, [conversations.data, mode, query, timezone])
  const groups = useMemo(() => groupByDay(items, timezone), [items, timezone])

  const save = useMutation({
    mutationFn: ({ conversation, title }: { conversation: Conversation; title: string }) => api.setConversationSaved(conversation.id, true, title),
    onSuccess: (conversation) => {
      updateCaches(cache, queryKey, conversation)
      setSaveTarget(null)
      notices.success(t('topicSaved'), conversation.title ?? undefined)
    },
    onError: (error) => notices.error(error),
  })
  const unsave = useMutation({
    mutationFn: (conversation: Conversation) => api.setConversationSaved(conversation.id, false),
    onSuccess: (conversation) => {
      cache.setQueryData<ConversationPages>(queryKey, (old) => old ? {
        ...old,
        pages: old.pages.map((page) => ({ ...page, conversations: page.conversations.filter((item) => item.id !== conversation.id) })),
      } : old)
      cache.setQueryData<ConversationPages>(['conversations'], (old) => replaceConversation(old, conversation))
      notices.success(t('topicRemoved'), conversation.title ?? undefined)
    },
    onError: (error) => notices.error(error),
  })
  const title = mode === 'saved' ? t('savedTopics') : t('pastDaysTitle')
  const hint = mode === 'saved' ? t('noSavedTopics') : t('pastDaysHint')
  const searchLabel = mode === 'saved' ? t('searchSavedTopics') : t('searchPastDays')

  return <AppPageShell active={mode} width="wide">
    <section className="archive-page" aria-labelledby="archive-title">
      <header className="archive-heading">
        <span className="archive-mark" aria-hidden="true">{mode === 'saved' ? <Bookmark /> : <CalendarDays />}</span>
        <div><span className="eyebrow">{mode === 'saved' ? t('savedTopic') : t('pastDays')}</span><h1 id="archive-title">{title}</h1><p>{hint}</p></div>
      </header>
      <label className="archive-search">
        <Search aria-hidden="true" />
        <span className="sr-only">{searchLabel}</span>
        <input value={query} onChange={(event) => setQuery(event.target.value)} placeholder={searchLabel} />
        {query && <kbd>{items.length}</kbd>}
      </label>
      <div className="archive-groups">
        {groups.map((group) => <section key={group.key} className="archive-day">
          <h2>{formatDayHeading(group.anchor, timezone)}</h2>
          <div className="timeline-list">
            {group.items.map((conversation) => <TimelineEntry
              key={conversation.id}
              conversation={conversation}
              timezone={timezone}
              onSave={mode === 'past' && !conversation.saved_at ? () => setSaveTarget(conversation) : undefined}
              onUnsave={mode === 'saved' ? () => unsave.mutate(conversation) : undefined}
            />)}
          </div>
        </section>)}
        {!conversations.isLoading && groups.length === 0 && <div className="archive-empty"><span aria-hidden="true">{mode === 'saved' ? <Bookmark /> : <CalendarDays />}</span><h2>{title}</h2><p>{mode === 'saved' ? t('noSavedTopics') : t('noPastActivity')}</p></div>}
      </div>
      {conversations.hasNextPage && <button className="load-more archive-load-more" onClick={() => void conversations.fetchNextPage()} disabled={conversations.isFetchingNextPage}>
        {conversations.isFetchingNextPage ? t('loadingMore') : t('loadMore')}
      </button>}
    </section>
    {saveTarget && <SaveTopicDialog
      initialTitle={saveTarget.title ?? saveTarget.preview ?? t('newConversation')}
      busy={save.isPending}
      onCancel={() => setSaveTarget(null)}
      onSave={(nextTitle) => save.mutate({ conversation: saveTarget, title: nextTitle })}
    />}
  </AppPageShell>
}

function groupByDay(items: Conversation[], timezone: string) {
  const groups = new Map<string, { key: string; anchor: string; items: Conversation[] }>()
  for (const item of items) {
    const date = new Date(item.last_activity_at)
    if (Number.isNaN(date.valueOf())) continue
    const key = dateKeyInTimeZone(date, timezone)
    const group = groups.get(key) ?? { key, anchor: item.last_activity_at, items: [] }
    group.items.push(item)
    groups.set(key, group)
  }
  return [...groups.values()]
    .sort((left, right) => right.key.localeCompare(left.key))
    .map((group) => ({ ...group, items: group.items.sort((left, right) => new Date(right.last_activity_at).valueOf() - new Date(left.last_activity_at).valueOf()) }))
}

function updateCaches(cache: ReturnType<typeof useQueryClient>, queryKey: readonly ['conversation-archive', 'saved' | 'past'], conversation: Conversation) {
  cache.setQueryData<ConversationPages>(queryKey, (old) => replaceConversation(old, conversation))
  cache.setQueryData<ConversationPages>(['conversations'], (old) => replaceConversation(old, conversation))
}
