import { useEffect, useMemo, useRef, useState } from 'react'
import { ArrowDown, CalendarClock, Menu, Sparkles } from 'lucide-react'
import { useInfiniteQuery, useMutation, useQuery, useQueryClient, type QueryClient } from '@tanstack/react-query'
import { useNavigate } from '@tanstack/react-router'
import { api, type ConversationEvent, type Message, type Run, type Snapshot, type TurnRating } from '../../api'
import { t } from '../../i18n'
import { describeError, useNotices, useQueryErrorNotice } from '../../notices'
import { ActivityTrail, ApprovalCard, ConversationMessage, StreamingMessage } from '../../components/ConversationMessage'
import { MobileBackButton } from '../../components/AppNavigation'
import { BrandMark } from '../../components/BrandMark'
import { Composer } from '../../components/Composer'
import { ConversationList } from '../../components/ConversationList'
import { ConversationMenu, DeleteConversationDialog, RenameConversationDialog } from '../../components/ConversationActions'
import { createClientId } from '../../shared/clientId'
import { dedupeConversations, prependConversation, removeConversation, replaceConversation, type ConversationPages } from '../../shared/conversations'
import { useAppChrome } from '../shell/useAppChrome'

type ConversationDialog = { kind: 'rename' } | { kind: 'delete' } | null
const ACTIVE_RUN_STATUSES = ['queued', 'running', 'waiting_for_approval', 'cancelling']
const TERMINAL_RUN_STATUSES = ['completed', 'failed', 'cancelled', 'interrupted']

export function ChatPage({ conversationId }: { conversationId: string | null }) {
  const cache = useQueryClient()
  const navigate = useNavigate()
  const notices = useNotices()
  const [draft, setDraft] = useState('')
  const [mobileConversation, setMobileConversation] = useState(Boolean(conversationId))
  const { navigation, navCollapsed, language, openMobileNavigation, session } = useAppChrome('conversations')
  const [streamedParts, setStreamedParts] = useState<Record<string, string[]>>({})
  const [streamedText, setStreamedText] = useState<Record<string, string>>({})
  const [progressItems, setProgressItems] = useState<Record<string, string[]>>({})
  const [showJump, setShowJump] = useState(false)
  const [dialog, setDialogState] = useState<ConversationDialog>(null)
  const streamedTextRef = useRef<Record<string, string>>({})
  const messageViewport = useRef<HTMLDivElement>(null)

  useEffect(() => setMobileConversation(Boolean(conversationId)), [conversationId])
  const conversations = useInfiniteQuery({
    queryKey: ['conversations'],
    queryFn: ({ pageParam }) => api.listConversations(pageParam || undefined),
    initialPageParam: '',
    getNextPageParam: (lastPage) => lastPage.next_cursor ?? undefined,
  })
  const conversationItems = useMemo(() => dedupeConversations(conversations.data?.pages.flatMap((page) => page.conversations) ?? []), [conversations.data])
  const commands = useQuery({ queryKey: ['commands', language], queryFn: () => api.commands(language), staleTime: Infinity })
  const selected = conversationId
  const snapshot = useQuery({
    queryKey: ['conversation', selected],
    queryFn: () => api.snapshot(selected!),
    enabled: selected !== null,
  })

  useEffect(() => {
    const cursor = snapshot.data?.event_cursor
    if (cursor === undefined) return
    const source = api.events(cursor, (event) => {
      updateEphemeralState(event, setStreamedText, setStreamedParts, setProgressItems, streamedTextRef)
      applyEvent(cache, event)
    }, () => {
      void cache.invalidateQueries({ queryKey: ['conversations'] })
      if (selected) void cache.invalidateQueries({ queryKey: ['conversation', selected] })
    })
    return () => source.close()
  }, [cache, selected, snapshot.data?.event_cursor])

  useEffect(() => {
    const viewport = messageViewport.current
    if (!viewport || showJump) return
    viewport.scrollTo({ top: viewport.scrollHeight, behavior: 'smooth' })
  }, [snapshot.data?.messages.length, streamedText, streamedParts, progressItems, showJump])

  const send = useMutation({
    mutationFn: ({ text, messageId }: { text: string; messageId: string }) => api.send(selected!, text, messageId),
    onMutate: ({ text, messageId }) => {
      cache.setQueryData<Snapshot>(['conversation', selected], (old) => old ? {
        ...old,
        messages: old.messages.some((message) => message.id === messageId) ? old.messages : [...old.messages, {
          id: messageId, role: 'user', content: text, created_at: new Date().toISOString(), compacted: false,
        }],
      } : old)
      setDraft('')
    },
    onError: (error, variables) => { setDraft(variables.text); notices.error(error, t('messageNotSent')) },
    onSettled: () => void cache.invalidateQueries({ queryKey: ['conversations'] }),
  })

  const createConversation = useMutation({
    mutationFn: api.createConversation,
    onSuccess: (conversation) => {
      cache.setQueryData<ConversationPages>(['conversations'], (old) => prependConversation(old, conversation))
      void navigate({ to: '/app/conversations/$conversationId', params: { conversationId: conversation.id } })
    },
    onError: (error) => notices.error(error),
  })
  const renameConversation = useMutation({
    mutationFn: ({ id, title }: { id: string; title: string }) => api.renameConversation(id, title),
    onSuccess: (conversation) => {
      cache.setQueryData<ConversationPages>(['conversations'], (old) => replaceConversation(old, conversation))
      cache.setQueryData<Snapshot>(['conversation', conversation.id], (old) => old ? { ...old, conversation } : old)
      setDialog(null)
      notices.success(t('conversationRenamed'), conversation.title ?? undefined)
    },
  })
  const deleteConversation = useMutation({
    mutationFn: (id: string) => api.deleteConversation(id),
    onSuccess: (_result, id) => {
      cache.setQueryData<ConversationPages>(['conversations'], (old) => removeConversation(old, id))
      cache.removeQueries({ queryKey: ['conversation', id] })
      setDialog(null)
      void navigate({ to: '/app/conversations', replace: true })
      notices.success(t('conversationDeleted'), conversationTitleRef.current)
    },
  })
  const setDialog = (next: ConversationDialog) => {
    renameConversation.reset(); deleteConversation.reset(); setDialogState(next)
  }
  const decideApproval = useMutation({
    mutationFn: ({ id, approved }: { id: string; approved: boolean }) => api.decideApproval(id, approved),
    onSuccess: (_result, { approved }) => {
      void cache.invalidateQueries({ queryKey: ['conversation', selected] })
      notices.info(approved ? t('approvalGranted') : t('approvalDenied'))
    },
    onError: (error) => notices.error(error),
  })
  const rateTurn = useMutation({
    mutationFn: ({ id, rating }: { id: string; rating: TurnRating }) => api.rateLatestTurn(id, rating),
    onMutate: ({ id, rating }) => {
      const previous = cache.getQueryData<Snapshot>(['conversation', id])?.latest_turn_rating ?? null
      cache.setQueryData<Snapshot>(['conversation', id], (old) => old ? { ...old, latest_turn_rating: rating } : old)
      return { previous }
    },
    onError: (error, { id }, context) => {
      cache.setQueryData<Snapshot>(['conversation', id], (old) => old ? { ...old, latest_turn_rating: context?.previous ?? null } : old)
      notices.error(error, t('feedbackNotSaved'))
    },
  })

  const active = snapshot.data?.runs.some((run) => ACTIVE_RUN_STATUSES.includes(run.status)) ?? false
  const visibleMessages = useMemo(() => snapshot.data?.messages.filter((message) => !message.compacted) ?? [], [snapshot.data?.messages])
  const latestReplyId = active ? null : [...visibleMessages].reverse().find((message) => message.role === 'assistant')?.id ?? null
  const conversation = snapshot.data?.conversation ?? conversationItems.find((item) => item.id === selected)
  const isSchedule = conversation?.kind === 'schedule'
  const title = conversation?.title ?? t('newConversation')
  const selectedProgress = selected ? progressItems[selected] ?? [] : []
  useQueryErrorNotice(session.error ?? conversations.error ?? snapshot.error)
  const conversationTitleRef = useRef<string>(title)
  useEffect(() => { conversationTitleRef.current = title }, [title])

  const submit = () => {
    const text = draft.trim()
    if (!selected || !text || active || send.isPending) return
    send.mutate({ text, messageId: createClientId() })
  }

  return <main className={`selu-shell${navCollapsed ? ' nav-collapsed' : ''}${mobileConversation ? ' mobile-chat-open' : ''}`}>
    {navigation}
    <ConversationList
      conversations={conversationItems} selectedId={selected} loading={conversations.isLoading} creating={createConversation.isPending}
      hasMore={conversations.hasNextPage} loadingMore={conversations.isFetchingNextPage}
      onLoadMore={() => void conversations.fetchNextPage()}
      onSelect={(id) => void navigate({ to: '/app/conversations/$conversationId', params: { conversationId: id } })}
      onCreate={() => createConversation.mutate()} onOpenNavigation={openMobileNavigation}
    />
    <section className="chat-workspace" aria-label={title}>
      <div className="ambient-light" aria-hidden="true" />
      <header className="chat-header">
        <MobileBackButton onClick={() => void navigate({ to: '/app/conversations' })} />
        <button className="icon-button mobile-menu chat-menu" onClick={openMobileNavigation} aria-label={t('openNavigation')}><Menu /></button>
        <div className="chat-heading"><span className="eyebrow">{isSchedule && <span className="kind-badge"><CalendarClock />{t('schedule')}</span>}{conversation?.channel_name ?? t('personalAgent')}</span><h1>{title}</h1></div>
        <div className={`presence-pill${active ? ' is-active' : ''}`}><span>{active ? t('working') : t('ready')}</span><i /></div>
        {selected && <ConversationMenu onRename={() => setDialog({ kind: 'rename' })} onDelete={() => setDialog({ kind: 'delete' })} />}
      </header>
      {!selected ? <WelcomeState onCreate={() => createConversation.mutate()} /> : <>
        <div className="message-viewport" ref={messageViewport} onScroll={(event) => {
          const node = event.currentTarget; setShowJump(node.scrollHeight - node.scrollTop - node.clientHeight > 180)
        }}><div className="message-column">
          {isSchedule && <div className="conversation-intro"><CalendarClock /><span>{t('scheduleHint')}</span></div>}
          {snapshot.isLoading && <MessageSkeleton />}
          {visibleMessages.map((message) => <ConversationMessage key={message.id} message={message} feedback={message.id === latestReplyId ? { rating: snapshot.data?.latest_turn_rating ?? null, busy: rateTurn.isPending, onRate: (rating) => rateTurn.mutate({ id: selected, rating }) } : undefined} />)}
          <ActivityTrail items={selectedProgress} active={active} />
          {snapshot.data?.pending_approval && <ApprovalCard approval={snapshot.data.pending_approval} busy={decideApproval.isPending} onDecision={(approved) => decideApproval.mutate({ id: snapshot.data!.pending_approval!.approval_id, approved })} />}
          <StreamingMessage parts={streamedParts[selected] ?? []} text={streamedText[selected] ?? ''} />
        </div></div>
        {showJump && <button className="jump-to-latest" onClick={() => { messageViewport.current?.scrollTo({ top: messageViewport.current.scrollHeight, behavior: 'smooth' }); setShowJump(false) }}><ArrowDown />{t('jumpToLatest')}</button>}
        <Composer value={draft} onChange={setDraft} onSend={submit} disabled={send.isPending || active} busy={active} commands={commands.data?.commands ?? []} />
      </>}
      {dialog?.kind === 'rename' && selected && <RenameConversationDialog initialTitle={conversation?.title ?? ''} busy={renameConversation.isPending} error={renameConversation.error ? describeError(renameConversation.error).body : null} onCancel={() => setDialog(null)} onSave={(next) => renameConversation.mutate({ id: selected, title: next })} />}
      {dialog?.kind === 'delete' && selected && <DeleteConversationDialog title={title} blocked={active} busy={deleteConversation.isPending} error={deleteConversation.error ? describeError(deleteConversation.error).body : null} onCancel={() => setDialog(null)} onConfirm={() => deleteConversation.mutate(selected)} />}
    </section>
  </main>
}

function WelcomeState({ onCreate }: { onCreate: () => void }) {
  return <div className="welcome-state"><div className="welcome-mark"><BrandMark animated /></div><span className="eyebrow">{t('yourPersonalAgent')}</span><h1>{t('welcomeTitle')}</h1><p>{t('welcomeBody')}</p><button className="primary-button" onClick={onCreate}>{t('startConversation')}<Sparkles /></button></div>
}
function MessageSkeleton() { return <div className="message-skeleton" aria-label={t('loading')}><i /><span><b /><b /><b /></span></div> }

function updateEphemeralState(event: ConversationEvent, setText: React.Dispatch<React.SetStateAction<Record<string, string>>>, setParts: React.Dispatch<React.SetStateAction<Record<string, string[]>>>, setProgress: React.Dispatch<React.SetStateAction<Record<string, string[]>>>, textRef: React.MutableRefObject<Record<string, string>>) {
  const id = event.conversation_id
  const clearText = () => setText((current) => { const next = { ...current, [id]: '' }; textRef.current = next; return next })
  const clearStream = () => { clearText(); setParts((current) => ({ ...current, [id]: [] })) }
  if (event.type === 'message.text_delta' && typeof event.payload.text === 'string') setText((current) => { const next = { ...current, [id]: `${current[id] ?? ''}${event.payload.text}` }; textRef.current = next; return next })
  else if (event.type === 'message.part_finished') { const part = textRef.current[id] ?? ''; if (part) setParts((current) => ({ ...current, [id]: [...(current[id] ?? []), part] })); clearText() }
  else if (event.type === 'run.progress' && typeof event.payload.label === 'string') setProgress((current) => ({ ...current, [id]: [...(current[id] ?? []), event.payload.label as string] }))
  else if (event.type === 'run.created') { setProgress((current) => ({ ...current, [id]: [] })); clearStream() }
  else if (event.type === 'run.output_finished' || event.type === 'run.error' || (event.type === 'run.updated' && TERMINAL_RUN_STATUSES.includes(String(event.payload.status)))) clearStream()
}

function applyEvent(cache: QueryClient, event: ConversationEvent) {
  if (event.type === 'conversation.deleted') { cache.setQueryData<ConversationPages>(['conversations'], (old) => removeConversation(old, event.conversation_id)); cache.removeQueries({ queryKey: ['conversation', event.conversation_id] }); return }
  let runEnded = false
  cache.setQueryData<Snapshot>(['conversation', event.conversation_id], (old) => {
    if (!old) return old
    if (event.type === 'message.created') { const message = event.payload.message as Message | undefined; if (message && !old.messages.some((existing) => existing.id === message.id)) return { ...old, messages: [...old.messages, message] } }
    if (event.type === 'run.created') { const run = event.payload.run as Run | undefined; if (run && !old.runs.some((existing) => existing.id === run.id)) return { ...old, runs: [...old.runs, run] } }
    if (event.type === 'run.updated') { const runId = event.payload.run_id; const status = event.payload.status; if (typeof runId === 'string' && typeof status === 'string') { runEnded = TERMINAL_RUN_STATUSES.includes(status); return { ...old, runs: old.runs.map((run) => run.id === runId ? { ...run, status } : run) } } }
    return old
  })
  if (event.type.startsWith('run.') || event.type === 'conversation.changed') void cache.invalidateQueries({ queryKey: ['conversations'] })
  if (runEnded || ['run.output_finished', 'run.error', 'conversation.changed', 'approval.requested'].includes(event.type)) void cache.invalidateQueries({ queryKey: ['conversation', event.conversation_id] })
}
