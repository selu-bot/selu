import { CalendarClock, Menu, MessageCircle, Plus, Search } from 'lucide-react'
import { useMemo, useState } from 'react'
import type { Conversation } from '../api'
import { t } from '../i18n'

type ConversationListProps = {
  conversations: Conversation[]
  selectedId: string | null
  loading: boolean
  creating: boolean
  hasMore: boolean
  loadingMore: boolean
  onLoadMore: () => void
  onSelect: (id: string) => void
  onCreate: () => void
  onOpenNavigation: () => void
}

export function ConversationList(props: ConversationListProps) {
  const { conversations, selectedId, loading, creating, hasMore, loadingMore, onLoadMore, onSelect, onCreate, onOpenNavigation } = props
  const [query, setQuery] = useState('')
  const filtered = useMemo(() => {
    const term = query.trim().toLocaleLowerCase()
    if (!term) return conversations
    return conversations.filter((item) => (item.title ?? t('newConversation')).toLocaleLowerCase().includes(term))
  }, [conversations, query])
  // Schedule threads collect the output of recurring runs. They are shown in
  // their own group so they never crowd out the conversations a person started.
  const regular = filtered.filter((item) => item.kind !== 'schedule')
  const scheduled = filtered.filter((item) => item.kind === 'schedule')

  return <section className={`conversation-panel${selectedId ? ' has-selection' : ''}`} aria-label={t('conversations')}>
    <header className="conversation-panel-header">
      <button className="icon-button mobile-menu" onClick={onOpenNavigation} aria-label={t('openNavigation')}><Menu /></button>
      <div><span className="eyebrow">{t('yourSpace')}</span><h1>{t('conversations')}</h1></div>
      <button className="new-chat-button" onClick={onCreate} disabled={creating} aria-label={t('newConversation')}><Plus /></button>
    </header>
    <label className="conversation-search">
      <Search />
      <span className="sr-only">{t('searchConversations')}</span>
      <input value={query} onChange={(event) => setQuery(event.target.value)} placeholder={t('searchConversations')} />
      {query && <kbd>{filtered.length}</kbd>}
    </label>
    <div className="conversation-scroll">
      {loading && <ConversationSkeleton />}
      {!loading && filtered.length === 0 && <div className="conversation-empty"><MessageCircle /><strong>{query ? t('nothingFound') : t('noConversations')}</strong><p>{query ? t('tryAnotherSearch') : t('startConversationHint')}</p></div>}
      {regular.map((conversation, index) => <ConversationItem key={conversation.id} conversation={conversation} index={index} selected={conversation.id === selectedId} onSelect={onSelect} />)}
      {scheduled.length > 0 && <>
        <div className="conversation-group-label"><CalendarClock aria-hidden="true" />{t('scheduledRuns')}</div>
        {scheduled.map((conversation, index) => <ConversationItem key={conversation.id} conversation={conversation} index={index} selected={conversation.id === selectedId} onSelect={onSelect} />)}
      </>}
      {hasMore && !query && <button className="load-more" onClick={onLoadMore} disabled={loadingMore}>
        {loadingMore ? t('loadingMore') : t('loadMore')}
      </button>}
    </div>
  </section>
}

type ConversationItemProps = {
  conversation: Conversation
  index: number
  selected: boolean
  onSelect: (id: string) => void
}

function ConversationItem({ conversation, index, selected, onSelect }: ConversationItemProps) {
  const isSchedule = conversation.kind === 'schedule'
  return <button
    className={`conversation-item stagger-${Math.min(index, 12)}${selected ? ' is-selected' : ''}${isSchedule ? ' is-schedule' : ''}`}
    onClick={() => onSelect(conversation.id)}
  >
    <span className="conversation-glyph">{isSchedule ? <CalendarClock aria-hidden="true" /> : initials(conversation.title)}</span>
    <span className="conversation-copy"><strong>{conversation.title ?? t('newConversation')}</strong><small>{conversation.channel_name}</small></span>
    <time dateTime={conversation.last_activity_at}>{formatRelative(conversation.last_activity_at)}</time>
    {conversation.active_run_id && <i className="conversation-live-dot" aria-label={t('working')} />}
  </button>
}

function ConversationSkeleton() {
  return <div className="conversation-skeleton" aria-label={t('loading')}>
    {[0, 1, 2, 3].map((item) => <span key={item}><i /><b /><small /></span>)}
  </div>
}

function initials(title: string | null) {
  return (title ?? 'S').split(/\s+/).slice(0, 2).map((part) => part[0]).join('').toLocaleUpperCase()
}

function formatRelative(value: string) {
  const date = new Date(value)
  if (Number.isNaN(date.valueOf())) return ''
  const seconds = Math.round((date.valueOf() - Date.now()) / 1000)
  const formatter = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto', style: 'narrow' })
  if (Math.abs(seconds) < 60) return formatter.format(seconds, 'second')
  const minutes = Math.round(seconds / 60)
  if (Math.abs(minutes) < 60) return formatter.format(minutes, 'minute')
  const hours = Math.round(minutes / 60)
  if (Math.abs(hours) < 24) return formatter.format(hours, 'hour')
  return new Intl.DateTimeFormat(undefined, { day: '2-digit', month: 'short' }).format(date)
}
