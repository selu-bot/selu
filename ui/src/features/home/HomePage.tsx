import { useMemo, useState, type FormEvent } from 'react'
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Link, useNavigate } from '@tanstack/react-router'
import { ArrowUp, Bookmark, CalendarClock, ChevronRight, Clock3, Menu, MessageCircle, Sparkles } from 'lucide-react'
import { api, type Conversation, type Message, type PhotoUpload, type Snapshot } from '../../api'
import { BrandMark } from '../../components/BrandMark'
import { SaveTopicDialog } from '../../components/ConversationActions'
import { PhotoPickerButton, PhotoPreviewStrip } from '../../components/Composer'
import { getLanguage, t, useLanguage } from '../../i18n'
import { useNotices } from '../../notices'
import { createClientId } from '../../shared/clientId'
import { dedupeConversations, replaceConversation, type ConversationPages } from '../../shared/conversations'
import { photoUploadPayload, preparePhotoFiles, type SelectedPhoto } from '../../shared/photoUploads'
import { failedSendQueryKey, type RetryableSend } from '../../shared/sendRetry'
import { navigateWithTransition } from '../../shared/transitions'
import { automationsApi, type Automation } from '../automations/api'
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
  const { navigation, navCollapsed, openMobileNavigation, session } = useAppChrome('home')
  const conversations = useInfiniteQuery({
    queryKey: ['conversations'],
    queryFn: ({ pageParam }) => api.listConversations(pageParam || undefined),
    initialPageParam: '',
    getNextPageParam: (lastPage) => lastPage.next_cursor ?? undefined,
  })
  const automations = useQuery({ queryKey: ['automations'], queryFn: automationsApi.list })
  const items = useMemo(
    () => dedupeConversations(conversations.data?.pages.flatMap((page) => page.conversations) ?? []),
    [conversations.data],
  )
  const today = useMemo(
    () => items.filter((item) => isSameLocalDay(item.last_activity_at, new Date())).sort(byNewestActivity),
    [items],
  )
  const upcoming = useMemo(
    () => (automations.data?.automations ?? [])
      .filter((item) => item.active && !Number.isNaN(new Date(item.next_run_at).valueOf()))
      .sort((left, right) => new Date(left.next_run_at).valueOf() - new Date(right.next_run_at).valueOf())
      .slice(0, 3),
    [automations.data],
  )

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
        <time className="today-date" dateTime={localDateKey(new Date())}>{formatDayHeading(new Date())}</time>
      </header>
      <div className="home-scroll">
        <div className="home-content today-content">
          <section className="ask-hero today-hero" aria-labelledby="home-title">
            <span className="eyebrow">{t('todayWithSelu')}</span>
            <h1 id="home-title">{name ? t('homeGreeting').replace('{name}', name) : t('homeGreetingFallback')}</h1>
            <p>{t('homeSubtitle')}</p>
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
              <textarea rows={2} value={draft} onChange={(event) => setDraft(event.target.value)} onKeyDown={(event) => {
                if (event.key === 'Enter' && !event.shiftKey) { event.preventDefault(); event.currentTarget.form?.requestSubmit() }
              }} placeholder={t('homePlaceholder')} aria-label={t('homePlaceholder')} disabled={start.isPending} />
              <button className="home-send-button" disabled={(!draft.trim() && photos.length === 0) || start.isPending} aria-label={t('send')}><ArrowUp /></button>
            </form>
            <span className="home-composer-hint">{t('homeComposerHint')}</span>
          </section>

          <div className="today-layout">
            <section className="today-timeline" aria-labelledby="today-timeline-title">
              <header className="today-section-heading">
                <div><span className="eyebrow">{t('today')}</span><h2 id="today-timeline-title">{t('todayWithSelu')}</h2><p>{t('todayTimelineHint')}</p></div>
                <Link to="/app/past" className="text-link">{t('pastDays')}<ChevronRight /></Link>
              </header>
              <div className="timeline-list">
                {today.map((item) => <TimelineEntry key={item.id} conversation={item} onSave={() => setSaveTarget(item)} />)}
                {!conversations.isLoading && today.length === 0 && <div className="timeline-empty"><Clock3 /><p>{t('noTodayActivity')}</p></div>}
              </div>
            </section>
            <aside className="upcoming-panel" aria-labelledby="upcoming-title">
              <header><CalendarClock aria-hidden="true" /><div><span className="eyebrow">{t('upcoming')}</span><h2 id="upcoming-title">{t('upcomingAutomations')}</h2></div></header>
              <div className="upcoming-list">
                {upcoming.map((automation) => <UpcomingAutomation key={automation.id} automation={automation} />)}
                {!automations.isLoading && upcoming.length === 0 && <p className="home-empty">{t('noUpcomingAutomations')}</p>}
              </div>
              <Link to="/app/automations" className="text-link upcoming-link">{t('schedules')}<ChevronRight /></Link>
            </aside>
          </div>
        </div>
      </div>
    </section>
    {saveTarget && <SaveTopicDialog
      initialTitle={saveTarget.title ?? topicNameFallback(saveTarget)}
      busy={saveTopic.isPending}
      onCancel={() => setSaveTarget(null)}
      onSave={(title) => saveTopic.mutate({ conversation: saveTarget, title })}
    />}
  </main>
}

export function TimelineEntry({ conversation, onSave, onUnsave }: { conversation: Conversation; onSave?: () => void; onUnsave?: () => void }) {
  const schedule = conversation.kind === 'schedule'
  const canSave = conversation.can_save ?? !schedule
  return <article className={`timeline-entry${schedule ? ' is-schedule' : ''}`}>
    <span className="timeline-rail" aria-hidden="true"><i /></span>
    <span className="timeline-icon" aria-hidden="true">{schedule ? <CalendarClock /> : <MessageCircle />}</span>
    <Link to="/app/conversations/$conversationId" params={{ conversationId: conversation.id }} className="timeline-copy" aria-label={`${t('openConversation')}: ${conversation.title ?? t('newConversation')}`}>
      <span className="timeline-meta"><time dateTime={conversation.last_activity_at}>{formatTime(conversation.last_activity_at)}</time>{schedule && <small>{t('scheduledResult')}</small>}{conversation.active_run_id && <small className="is-live">{t('working')}</small>}</span>
      <strong>{conversation.title ?? t('newConversation')}</strong>
      {conversation.preview && <p>{conversation.preview}</p>}
    </Link>
    {canSave && !conversation.saved_at && onSave && <button className="topic-action" type="button" onClick={onSave}><Bookmark />{t('saveTopic')}</button>}
    {canSave && conversation.saved_at && onUnsave && <button className="topic-action is-saved" type="button" onClick={onUnsave}><Bookmark />{t('unsaveTopic')}</button>}
    {canSave && conversation.saved_at && !onUnsave && <span className="saved-badge"><Bookmark />{t('savedTopic')}</span>}
  </article>
}

function UpcomingAutomation({ automation }: { automation: Automation }) {
  return <Link to="/app/automations" className="upcoming-item">
    <span><strong>{automation.name}</strong><small>{automation.timing.description}</small></span>
    <time dateTime={automation.next_run_at}><small>{t('nextRun')}</small>{formatUpcoming(automation.next_run_at)}</time>
  </Link>
}

function byNewestActivity(left: Conversation, right: Conversation) {
  return new Date(right.last_activity_at).valueOf() - new Date(left.last_activity_at).valueOf()
}

export function localDateKey(date: Date) {
  const year = date.getFullYear()
  const month = String(date.getMonth() + 1).padStart(2, '0')
  const day = String(date.getDate()).padStart(2, '0')
  return `${year}-${month}-${day}`
}

export function isSameLocalDay(value: string, reference: Date) {
  const date = new Date(value)
  return !Number.isNaN(date.valueOf()) && localDateKey(date) === localDateKey(reference)
}

function selectedLocale() {
  return getLanguage() === 'de' ? 'de-DE' : 'en-US'
}

export function formatDayHeading(date: Date) {
  return new Intl.DateTimeFormat(selectedLocale(), { weekday: 'long', month: 'long', day: 'numeric' }).format(date)
}

function formatTime(value: string) {
  const date = new Date(value)
  if (Number.isNaN(date.valueOf())) return ''
  return new Intl.DateTimeFormat(selectedLocale(), { hour: 'numeric', minute: '2-digit' }).format(date)
}

function formatUpcoming(value: string) {
  const date = new Date(value)
  if (Number.isNaN(date.valueOf())) return ''
  return new Intl.DateTimeFormat(selectedLocale(), { weekday: 'short', hour: 'numeric', minute: '2-digit' }).format(date)
}

function topicNameFallback(conversation: Conversation) {
  const preview = conversation.preview?.trim()
  if (!preview) return t('newConversation')
  return preview.length > 60 ? `${preview.slice(0, 57)}…` : preview
}
