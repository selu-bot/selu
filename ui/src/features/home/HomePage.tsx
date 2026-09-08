import { useMemo, useState, type FormEvent } from 'react'
import { useInfiniteQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { Link, useNavigate } from '@tanstack/react-router'
import { AlertCircle, ArrowUp, CalendarClock, Clock3, Menu, MessageCircle, Sparkles } from 'lucide-react'
import { api, type Conversation, type Message, type PhotoUpload, type Snapshot } from '../../api'
import { BrandMark } from '../../components/BrandMark'
import { PhotoPickerButton, PhotoPreviewStrip } from '../../components/Composer'
import { t } from '../../i18n'
import { useNotices } from '../../notices'
import { createClientId } from '../../shared/clientId'
import { dedupeConversations, prependConversation, type ConversationPages } from '../../shared/conversations'
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
  const cache = useQueryClient()
  const navigate = useNavigate()
  const notices = useNotices()
  const [draft, setDraft] = useState('')
  const [photos, setPhotos] = useState<SelectedPhoto[]>([])
  const [photoSelectionBusy, setPhotoSelectionBusy] = useState(false)
  const { navigation, navCollapsed, openMobileNavigation, session } = useAppChrome('home')
  const conversations = useInfiniteQuery({
    queryKey: ['conversations'],
    queryFn: ({ pageParam }) => api.listConversations(pageParam || undefined),
    initialPageParam: '',
    getNextPageParam: (lastPage) => lastPage.next_cursor ?? undefined,
  })
  const items = useMemo(() => dedupeConversations(conversations.data?.pages.flatMap((page) => page.conversations) ?? []), [conversations.data])
  const needsAttention = items.filter((item) => item.active_run_id).slice(0, 3)
  const upcoming = items.filter((item) => item.kind === 'schedule').slice(0, 3)
  const recent = items.filter((item) => item.kind !== 'schedule' && !item.active_run_id).slice(0, 5)

  const start = useMutation({
    mutationFn: ({ text, selectedPhotos }: { text: string; selectedPhotos: SelectedPhoto[] }) => startHomeConversation({
      text,
      attachments: photoUploadPayload(selectedPhotos),
      optimisticAttachments: selectedPhotos.map(({ filename, mime_type, preview_url, size_bytes }) => ({ filename, mime_type, preview_url, size_bytes })),
      create: api.createConversation,
      send: api.send,
      showOptimistically: (conversation, message) => {
        cache.setQueryData<ConversationPages>(['conversations'], (old) => prependConversation(old, conversation))
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
        <Link to="/app/conversations" className="home-all-link">{t('allConversations')}</Link>
      </header>
      <div className="home-scroll">
        <div className="home-content">
          <section className="ask-hero" aria-labelledby="home-title">
            <span className="eyebrow">{t('yourPersonalAgent')}</span>
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

          <div className="home-sections">
            <HomeSection title={t('needsAttention')} subtitle={t('needsAttentionHint')} icon={AlertCircle} items={needsAttention} empty={t('nothingNeedsAttention')} tone="attention" />
            <HomeSection title={t('upcoming')} subtitle={t('upcomingHint')} icon={CalendarClock} items={upcoming} empty={t('nothingUpcoming')} tone="upcoming" />
            <HomeSection title={t('recent')} subtitle={t('recentHint')} icon={Clock3} items={recent} empty={t('noRecentConversations')} tone="recent" />
          </div>
        </div>
      </div>
    </section>
  </main>
}

function HomeSection({ title, subtitle, icon: Icon, items, empty, tone }: { title: string; subtitle: string; icon: typeof Clock3; items: Conversation[]; empty: string; tone: string }) {
  return <section className={`home-section is-${tone}`}>
    <header><span><Icon aria-hidden="true" /></span><div><h2>{title}</h2><p>{subtitle}</p></div></header>
    <div className="home-card-list">
      {items.map((item) => <Link key={item.id} to="/app/conversations/$conversationId" params={{ conversationId: item.id }} className="home-conversation-card">
        <span className="home-card-icon">{item.kind === 'schedule' ? <CalendarClock /> : <MessageCircle />}</span>
        <span><strong>{item.title ?? t('newConversation')}</strong><small>{item.channel_name}</small></span>
        <time dateTime={item.last_activity_at}>{formatDate(item.last_activity_at)}</time>
      </Link>)}
      {items.length === 0 && <p className="home-empty">{empty}</p>}
    </div>
  </section>
}

function formatDate(value: string) {
  const date = new Date(value)
  if (Number.isNaN(date.valueOf())) return ''
  return new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' }).format(date)
}
