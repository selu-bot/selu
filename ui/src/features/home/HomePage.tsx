import { useEffect, useMemo, useState, type FormEvent, type ReactNode } from 'react'
import { useInfiniteQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { Link, useNavigate } from '@tanstack/react-router'
import { ArrowUp, Bookmark, CalendarClock, CalendarSearch, ChevronRight, Clock3, Menu, MessageCircle, Sparkles } from 'lucide-react'
import { api, type Conversation, type ConversationPage, type Message, type PhotoUpload, type Snapshot } from '../../api'
import { BrandMark } from '../../components/BrandMark'
import { SaveTopicDialog } from '../../components/ConversationActions'
import { PhotoPickerButton, PhotoPreviewStrip } from '../../components/Composer'
import { getLanguage, t, useLanguage } from '../../i18n'
import { useNotices } from '../../notices'
import { createClientId } from '../../shared/clientId'
import { dateKeyInTimeZone, formatInTimeZone, isSameDayInTimeZone } from '../../shared/dateTime'
import { dedupeConversations, replaceConversation, type ConversationPages } from '../../shared/conversations'
import { photoUploadPayload, preparePhotoFiles, type SelectedPhoto } from '../../shared/photoUploads'
import { failedSendQueryKey, type RetryableSend } from '../../shared/sendRetry'
import { navigateWithTransition } from '../../shared/transitions'
import { useAppChrome } from '../shell/useAppChrome'

type StartConversationDependencies = {
  text: string
  attachments?: PhotoUpload[]
  optimisticAttachments?: Message['attachments']
  create: () => Promise<Conversation>
  send: (id: string, text: string, messageId: string, attachments?: PhotoUpload[]) => Promise<unknown>
  showOptimistically: (conversation: Conversation, message: Message) => void
  navigate: (conversationId: string) => void | Promise<void>
  onSendError?: (conversation: Conversation, messageId: string) => void
  messageId?: () => string
}

export async function startHomeConversation(input: StartConversationDependencies) {
  const messageId = input.messageId?.() ?? createClientId()
  const conversation = await input.create()
  const message: Message = {
    id: messageId,
    role: 'user',
    content: input.text,
    created_at: new Date().toISOString(),
    compacted: false,
    attachments: input.optimisticAttachments,
  }
  input.showOptimistically(conversation, message)
  const sendResult = input.send(conversation.id, input.text, message.id, input.attachments ?? []).then(
    () => ({ ok: true as const }),
    (error: unknown) => ({ ok: false as const, error }),
  )
  await input.navigate(conversation.id)
  const result = await sendResult
  if (!result.ok) {
    input.onSendError?.(conversation, messageId)
    throw result.error
  }
  return conversation
}

export function HomePage() {
  useLanguage()
  const cache = useQueryClient()
  const navigate = useNavigate()
  const notices = useNotices()
  const [draft, setDraft] = useState('')
  const [photos, setPhotos] = useState<SelectedPhoto[]>([])
  const [photoSelectionBusy, setPhotoSelectionBusy] = useState(false)
  const [saveTarget, setSaveTarget] = useState<Conversation | null>(null)
  const [now, setNow] = useState(Date.now)
  const { navigation, navCollapsed, openMobileNavigation, session } = useAppChrome('home')
  const conversations = useInfiniteQuery({
    queryKey: ['conversations'],
    queryFn: ({ pageParam }) => api.listConversations(pageParam || undefined),
    initialPageParam: '',
    getNextPageParam: (lastPage) => lastPage.next_cursor ?? undefined,
  })
  const items = useMemo(
    () => dedupeConversations(conversations.data?.pages.flatMap((page) => page.conversations) ?? []),
    [conversations.data],
  )
  const timezone = session.data?.timezone ?? 'UTC'
  const completedToday = useMemo(
    () => completedTodayConversations(items, now, timezone),
    [items, now, timezone],
  )
  const loadNextTodayPage = shouldLoadNextHomePage(conversations.data?.pages ?? [], now, timezone)

  useEffect(() => {
    if (loadNextTodayPage && !conversations.isFetchingNextPage && !conversations.isError) {
      void conversations.fetchNextPage()
    }
  }, [conversations.fetchNextPage, conversations.isError, conversations.isFetchingNextPage, loadNextTodayPage])

  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 60_000)
    return () => window.clearInterval(timer)
  }, [])

  const start = useMutation({
    mutationFn: ({ text, selectedPhotos }: { text: string; selectedPhotos: SelectedPhoto[] }) => startHomeConversation({
      text,
      attachments: photoUploadPayload(selectedPhotos),
      optimisticAttachments: selectedPhotos.map(({ filename, mime_type, preview_url, size_bytes }) => ({ filename, mime_type, preview_url, size_bytes })),
      create: api.createConversation,
      send: api.send,
      // Keep the provisional conversation out of list caches. Chat still gets
      // the immediate optimistic message while send acceptance is pending.
      showOptimistically: (conversation, message) => {
        cache.setQueryData<Snapshot>(['conversation', conversation.id], {
          conversation, messages: [message], runs: [], pending_approval: null, latest_turn_rating: null, event_cursor: 0,
        })
      },
      navigate: (conversationId) => navigateWithTransition(() => navigate({ to: '/app/conversations/$conversationId', params: { conversationId } })),
      onSendError: (conversation, messageId) => {
        const retry: RetryableSend = { text, messageId, photos: selectedPhotos }
        cache.setQueryData(failedSendQueryKey(conversation.id), retry)
      },
    }),
    onSuccess: () => {
      setDraft('')
      setPhotos([])
      void cache.invalidateQueries({ queryKey: ['conversations'] })
    },
    onError: (error) => notices.error(error, t('messageNotSent')),
    onSettled: (_data) => {
      const id = _data?.id
      if (id) void cache.invalidateQueries({ queryKey: ['conversation', id] })
    },
  })
  const saveTopic = useMutation({
    mutationFn: ({ conversation, title }: { conversation: Conversation; title: string }) => api.setConversationSaved(conversation.id, true, title),
    onSuccess: (conversation) => {
      cache.setQueryData<ConversationPages>(['conversations'], (old) => replaceConversation(old, conversation))
      cache.setQueryData<Snapshot>(['conversation', conversation.id], (old) => old ? { ...old, conversation } : old)
      setSaveTarget(null)
      notices.success(t('topicSaved'), conversation.title ?? undefined)
    },
    onError: (error) => notices.error(error),
  })
  const addPhotos = async (files: File[]) => {
    setPhotoSelectionBusy(true)
    try {
      const prepared = await preparePhotoFiles(files, photos)
      setPhotos((current) => [...current, ...prepared])
    } catch (error) {
      notices.error(error, t('photosNotAdded'))
    } finally {
      setPhotoSelectionBusy(false)
    }
  }
  const submit = (event: FormEvent) => {
    event.preventDefault()
    const text = draft.trim()
    if ((text || photos.length > 0) && !start.isPending) start.mutate({ text, selectedPhotos: photos })
  }
  const name = session.data?.display_name?.trim().split(/\s+/)[0]

  return <main className={`home-shell${navCollapsed ? ' nav-collapsed' : ''}`}>
    {navigation}
    <section className="home-page">
      <header className="home-topbar">
        <button className="icon-button mobile-menu" onClick={openMobileNavigation} aria-label={t('openNavigation')}><Menu /></button>
        <BrandMark compact />
        <time className="today-date" dateTime={dateKeyInTimeZone(now, timezone)}>{formatDayHeading(now, timezone)}</time>
      </header>
      <div className="home-scroll">
        <div className="home-content today-content">
          <section className="today-timeline dayline" aria-labelledby="today-timeline-title">
            <header className="today-section-heading">
              <div className="today-heading-copy">
                <span className="eyebrow">{t('today')}</span>
                <h1 id="today-timeline-title">{t('todayWithSelu')}</h1>
                <p>{name ? t('homeGreeting').replace('{name}', name) : t('homeGreetingFallback')}</p>
              </div>
              <nav className="dayline-links" aria-label={t('todayWithSelu')}>
                <Link to="/app/past" className="dayline-search-link" aria-label={t('searchPastDays')} title={t('searchPastDays')}>
                  <CalendarSearch aria-hidden="true" /><span>{t('pastDays')}</span><ChevronRight aria-hidden="true" />
                </Link>
                <Link to="/app/automations" className="text-link"><CalendarClock aria-hidden="true" />{t('schedules')}<ChevronRight aria-hidden="true" /></Link>
              </nav>
            </header>
            <div className="timeline-list dayline-list">
              {completedToday.map((item) => <TimelineEntry key={item.id} conversation={item} timezone={timezone} onSave={() => setSaveTarget(item)} />)}
              {!conversations.isLoading && completedToday.length === 0 && <DaylineEmpty icon={<Clock3 />} label={t('noTodayActivity')} />}
            </div>
          </section>
        </div>
      </div>
      <footer className="home-quick-compose" aria-label={t('homePlaceholder')}>
        <form className="home-composer" onSubmit={submit}>
          <PhotoPreviewStrip photos={photos} disabled={start.isPending} onRemove={(id) => setPhotos((current) => current.filter((photo) => photo.id !== id))} />
          <div className="home-composer-tools">
            <Sparkles aria-hidden="true" />
            {session.data?.supports_photo_uploads === true && <PhotoPickerButton
              className="home-photo-picker"
              disabled={start.isPending || photoSelectionBusy}
              onSelect={(files) => void addPhotos(files)}
            />}
          </div>
          <textarea rows={1} value={draft} onChange={(event) => setDraft(event.target.value)} onKeyDown={(event) => {
            if (event.key === 'Enter' && !event.shiftKey) { event.preventDefault(); event.currentTarget.form?.requestSubmit() }
          }} placeholder={t('homePlaceholder')} aria-label={t('homePlaceholder')} disabled={start.isPending} />
          <button className="home-send-button" disabled={(!draft.trim() && photos.length === 0) || start.isPending} aria-label={t('send')}><ArrowUp /></button>
        </form>
        <span className="home-composer-hint">{t('homeComposerHint')}</span>
      </footer>
    </section>
    {saveTarget && <SaveTopicDialog
      initialTitle={saveTarget.title ?? topicNameFallback(saveTarget)}
      busy={saveTopic.isPending}
      onCancel={() => setSaveTarget(null)}
      onSave={(title) => saveTopic.mutate({ conversation: saveTarget, title })}
    />}
  </main>
}

export function canSaveTopic(conversation: Conversation) {
  return conversation.kind !== 'schedule' && (conversation.can_save ?? true)
}

export function TimelineEntry({ conversation, timezone, onSave, onUnsave }: { conversation: Conversation; timezone: string; onSave?: () => void; onUnsave?: () => void }) {
  const schedule = conversation.kind === 'schedule'
  const canSave = canSaveTopic(conversation)
  return <article className={`timeline-entry${schedule ? ' is-schedule' : ''}`}>
    <time className="timeline-time" dateTime={conversation.last_activity_at}>{formatTime(conversation.last_activity_at, timezone)}</time>
    <span className="timeline-rail" aria-hidden="true"><i>{schedule ? <CalendarClock /> : <MessageCircle />}</i></span>
    <Link to="/app/conversations/$conversationId" params={{ conversationId: conversation.id }} className="timeline-copy" aria-label={`${t('openConversation')}: ${conversation.title ?? t('newConversation')}`}>
      <span className="timeline-meta">
        {schedule && <small className="is-schedule-state">{t('scheduledResult')}</small>}
        {conversation.saved_at && <small className="is-saved-state"><Bookmark aria-hidden="true" />{t('savedTopic')}</small>}
        {conversation.active_run_id && <small className="is-live">{t('working')}</small>}
      </span>
      <strong>{conversation.title ?? t('newConversation')}</strong>
      {conversation.preview && <p>{conversation.preview}</p>}
      <ChevronRight className="timeline-open-cue" aria-hidden="true" />
    </Link>
    {canSave && !conversation.saved_at && onSave && <button className="topic-action" type="button" onClick={onSave}><Bookmark aria-hidden="true" />{t('saveTopic')}</button>}
    {canSave && conversation.saved_at && onUnsave && <button className="topic-action is-saved" type="button" onClick={onUnsave}><Bookmark aria-hidden="true" />{t('unsaveTopic')}</button>}
  </article>
}

function DaylineEmpty({ icon, label }: { icon: ReactNode; label: string }) {
  return <div className="dayline-empty">
    <span className="timeline-time" aria-hidden="true" />
    <span className="timeline-rail" aria-hidden="true"><i>{icon}</i></span>
    <p>{label}</p>
  </div>
}

function byNewestActivity(left: Conversation, right: Conversation) {
  return new Date(right.last_activity_at).valueOf() - new Date(left.last_activity_at).valueOf()
}

export function completedTodayConversations(items: Conversation[], now: string | number | Date, timezone: string) {
  const upperBound = new Date(now).valueOf()
  return items
    .filter((item) => !item.active_run_id
      && new Date(item.last_activity_at).valueOf() <= upperBound
      && isSameDayInTimeZone(item.last_activity_at, now, timezone))
    .sort(byNewestActivity)
}

export function shouldLoadNextHomePage(pages: ConversationPage[], now: string | number | Date, timezone: string) {
  const lastPage = pages.at(-1)
  const nextCursor = lastPage?.next_cursor
  if (!lastPage || !nextCursor) return false
  if (pages.slice(0, -1).some((page) => page.next_cursor === nextCursor)) return false

  const oldestLoaded = lastPage.conversations.at(-1)
  if (!oldestLoaded) return true
  const oldestKey = dateKeyInTimeZone(oldestLoaded.last_activity_at, timezone)
  const todayKey = dateKeyInTimeZone(now, timezone)
  return !oldestKey || !todayKey || oldestKey >= todayKey
}

function selectedLocale() {
  return getLanguage() === 'de' ? 'de-DE' : 'en-US'
}

export function formatDayHeading(value: string | number | Date, timezone: string) {
  return formatInTimeZone(value, selectedLocale(), timezone, { weekday: 'long', month: 'long', day: 'numeric' })
}

function formatTime(value: string, timezone: string) {
  return formatInTimeZone(value, selectedLocale(), timezone, { hour: 'numeric', minute: '2-digit' })
}

function topicNameFallback(conversation: Conversation) {
  const preview = conversation.preview?.trim()
  if (!preview) return t('newConversation')
  return preview.length > 60 ? `${preview.slice(0, 57)}…` : preview
}
