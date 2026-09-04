import { AlertTriangle, CheckCircle2, Info, X } from 'lucide-react'
import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from 'react'
import { ApiError } from './api'
import { t, type TranslationKey } from './i18n'

/**
 * Notices are the SPA's single feedback channel for things that happen outside
 * the user's direct line of sight: a request that failed, an action that
 * completed, or something worth knowing. They are transient, stack in one
 * fixed region, can always be dismissed, and never block the interface.
 *
 * Use them for outcomes, not for state. Ongoing work belongs to the element
 * that started it (spinner on a button, disabled composer, presence pill).
 * Validation belongs inline next to the field. A confirmation belongs in a
 * dialog. When a dialog is open and its own action fails, show the error in
 * the dialog and skip the notice so the message appears where the user acted.
 */
export type NoticeKind = 'success' | 'info' | 'error'

export type Notice = {
  id: number
  kind: NoticeKind
  title: string
  body?: string
}

export type NoticeInput = Omit<Notice, 'id'> & { duration?: number }

const DURATION_MS: Record<NoticeKind, number> = { success: 4_000, info: 6_000, error: 8_000 }
const MAX_VISIBLE = 3

type NoticeApi = {
  notify: (notice: NoticeInput) => number
  success: (title: string, body?: string) => number
  info: (title: string, body?: string) => number
  error: (error: unknown, fallbackTitle?: string) => number
  dismiss: (id: number) => void
}

const NoticeContext = createContext<NoticeApi | null>(null)

export function NoticeProvider({ children }: { children: React.ReactNode }) {
  const [notices, setNotices] = useState<Notice[]>([])
  const timers = useRef(new Map<number, number>())
  const sequence = useRef(0)

  const dismiss = useCallback((id: number) => {
    const timer = timers.current.get(id)
    if (timer !== undefined) window.clearTimeout(timer)
    timers.current.delete(id)
    setNotices((current) => current.filter((notice) => notice.id !== id))
  }, [])

  const notify = useCallback((input: NoticeInput) => {
    const id = ++sequence.current
    const { duration = DURATION_MS[input.kind], ...rest } = input
    setNotices((current) => [...current, { id, ...rest }].slice(-MAX_VISIBLE))
    timers.current.set(id, window.setTimeout(() => dismiss(id), duration))
    return id
  }, [dismiss])

  useEffect(() => () => timers.current.forEach((timer) => window.clearTimeout(timer)), [])

  const api = useMemo<NoticeApi>(() => ({
    notify,
    dismiss,
    success: (title, body) => notify({ kind: 'success', title, body }),
    info: (title, body) => notify({ kind: 'info', title, body }),
    error: (error, fallbackTitle) => notify({ kind: 'error', ...describeError(error, fallbackTitle) }),
  }), [notify, dismiss])

  return <NoticeContext.Provider value={api}>
    {children}
    <NoticeStack notices={notices} onDismiss={dismiss} />
  </NoticeContext.Provider>
}

export function useNotices(): NoticeApi {
  const api = useContext(NoticeContext)
  if (!api) throw new Error('useNotices must be used inside <NoticeProvider>')
  return api
}

/**
 * Report a query error exactly once per failure. Queries keep their error
 * until they succeed again, so a plain effect would re-announce on every
 * render; this only fires when the error object changes.
 */
export function useQueryErrorNotice(error: unknown, fallbackTitle?: string) {
  const notices = useNotices()
  const last = useRef<unknown>(null)
  useEffect(() => {
    if (!error || error === last.current) return
    last.current = error
    notices.error(error, fallbackTitle)
  }, [error, fallbackTitle, notices])
}

const ICONS: Record<NoticeKind, typeof Info> = { success: CheckCircle2, info: Info, error: AlertTriangle }

function NoticeStack({ notices, onDismiss }: { notices: Notice[]; onDismiss: (id: number) => void }) {
  return <div className="notice-region" aria-label={t('notifications')}>
    {notices.map((notice) => {
      const Icon = ICONS[notice.kind]
      return <div
        key={notice.id}
        className={`notice is-${notice.kind}`}
        role={notice.kind === 'error' ? 'alert' : 'status'}
        aria-live={notice.kind === 'error' ? 'assertive' : 'polite'}
      >
        <Icon aria-hidden="true" />
        <span><strong>{notice.title}</strong>{notice.body && <small>{notice.body}</small>}</span>
        <button className="notice-close" onClick={() => onDismiss(notice.id)} aria-label={t('dismiss')}><X /></button>
      </div>
    })}
  </div>
}

const KNOWN_CODES: Record<string, TranslationKey> = {
  'conversation.run_in_progress': 'errorRunInProgress',
  'conversation.approval_expired': 'errorApprovalExpired',
  'conversation.invalid_title': 'errorInvalidTitle',
  'session.expired': 'errorSessionExpired',
}

/** Translate any thrown value into a title and plain-language explanation. */
export function describeError(error: unknown, fallbackTitle: string = t('somethingWentWrong')): { title: string; body: string } {
  const title = fallbackTitle
  if (error instanceof ApiError) {
    if (error.code && KNOWN_CODES[error.code]) return { title, body: t(KNOWN_CODES[error.code]) }
    if (error.status === 404 || error.status === 405) return { title, body: t('errorNotAvailable') }
    if (error.status >= 500) return { title, body: t('errorServer') }
  }
  if (error instanceof TypeError) return { title, body: t('errorOffline') }
  return { title, body: t('tryAgain') }
}
