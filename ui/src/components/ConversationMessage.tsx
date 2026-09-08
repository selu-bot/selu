import { Check, ChevronDown, Copy, ShieldCheck, Sparkles, ThumbsDown, ThumbsUp, Wrench } from 'lucide-react'
import { useState } from 'react'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import type { Approval, Message, MessageAttachment, TurnRating } from '../api'
import { defineTranslations, t, useTranslations } from '../i18n'
import { appPath } from '../shared/paths'
import { BrandMark } from './BrandMark'

const PHOTO_MIME_TYPES = new Set(['image/jpeg', 'image/png', 'image/gif', 'image/webp'])
const ATTACHMENT_CONTEXT_MARKER = '\n\nAttached image artifacts:\n'
const PHOTO_ONLY_CONTEXT = 'User sent image attachment(s) without accompanying text.'
const TOOL_PLACEHOLDER_PATTERN = /^\s*(?:\[calling [^\]\r\n]+\]\s*)+$/
const TOOL_NAME_PATTERN = /\[calling ([^\]\r\n]+)\]/g

const toolActivityTranslations = defineTranslations(
  {
    calendar: 'Worked with the calendar',
    delegation: 'Brought in additional help',
    document: 'Worked with a document',
    email: 'Worked on an email',
    image: 'Worked with an image',
    location: 'Looked up a place',
    memory: 'Checked saved information',
    other: 'Completed another step',
    weather: 'Checked the weather',
    webSearch: 'Searched the web',
  },
  {
    calendar: 'Mit dem Kalender gearbeitet',
    delegation: 'Weitere Unterstützung hinzugezogen',
    document: 'Mit einem Dokument gearbeitet',
    email: 'An einer E-Mail gearbeitet',
    image: 'Mit einem Bild gearbeitet',
    location: 'Einen Ort nachgeschlagen',
    memory: 'Gespeicherte Informationen geprüft',
    other: 'Einen weiteren Schritt erledigt',
    weather: 'Das Wetter geprüft',
    webSearch: 'Im Web gesucht',
  },
)

type ToolActivityCopy = { [K in keyof typeof toolActivityTranslations.en]: string }

/// Thumbs feedback is offered on the latest reply only, because the rating is
/// stored on the most recent turn and feeds the agent's behavioral lessons.
export type TurnFeedback = {
  rating: number | null
  busy: boolean
  onRate: (rating: TurnRating) => void
}

type ConversationMessageProps = {
  message: Message
  entering?: boolean
  feedback?: TurnFeedback
}

export function ConversationMessage({ message, entering = false, feedback }: ConversationMessageProps) {
  const activities = toolActivityNames(message)
  if (activities) return <ToolActivities names={activities} />
  if (message.role === 'tool') return <ToolMessage message={message} />
  if (message.role === 'system') return null
  const photos = displayablePhotos(message.attachments)
  const content = displayContent(message)
  const hasText = Boolean(content.trim())
  if (!hasText && photos.length === 0) return null
  return <article className={`message-row is-${message.role}${photos.length ? ' has-attachments' : ''}${entering ? ' is-entering' : ''}`}>
    {message.role === 'assistant' && <div className="message-avatar"><BrandMark compact /></div>}
    <div className="message-content">
      <div className="message-meta">
        <span>{message.role === 'user' ? t('you') : 'Selu'}</span>
        <time dateTime={message.created_at}>{formatTime(message.created_at)}</time>
      </div>
      <PhotoAttachments photos={photos} />
      {hasText && <div className="message-surface">
        <Markdown>{content}</Markdown>
      </div>}
      {message.role === 'assistant' && <div className="message-actions">
        {hasText && <CopyButton text={content} />}
        {feedback && <FeedbackButtons feedback={feedback} />}
      </div>}
    </div>
  </article>
}

function ToolActivities({ names }: { names: string[] }) {
  const copy = useTranslations(toolActivityTranslations)
  return <div className="tool-activity-list" role="list" aria-label={t('workDone')}>
    {names.map((name, index) => <div className="tool-activity-item" role="listitem" key={`${name}-${index}`}>
      <span className="tool-activity-check" aria-hidden="true"><Check /></span>
      <span>{toolActivityLabel(name, copy)}</span>
    </div>)}
  </div>
}

function toolActivityNames(message: Message): string[] | null {
  if (message.role !== 'assistant' || !TOOL_PLACEHOLDER_PATTERN.test(message.content)) return null
  const structured = structuredToolNames(message.tool_calls)
  if (structured.length) return structured
  return [...message.content.matchAll(TOOL_NAME_PATTERN)].map((match) => match[1].trim())
}

function structuredToolNames(value: unknown): string[] {
  if (!Array.isArray(value)) return []
  return value.flatMap((item) => {
    if (typeof item !== 'object' || item === null) return []
    const name = (item as { name?: unknown }).name
    return typeof name === 'string' && name.trim() ? [name.trim()] : []
  })
}

function toolActivityLabel(toolName: string, copy: ToolActivityCopy): string {
  const name = toolName.toLowerCase()
  if (name.includes('delegate')) return copy.delegation
  if (name.includes('memory') || name.includes('knowledge')) return copy.memory
  if (name.includes('location') || name.includes('geocode') || name.includes('address') || name.includes('map')) return copy.location
  if (name.includes('email') || name.includes('mail') || name.includes('inbox')) return copy.email
  if (name.includes('calendar') || name.includes('event')) return copy.calendar
  if (name.includes('weather') || name.includes('forecast')) return copy.weather
  if (name.includes('image') || name.includes('photo') || name.includes('vision')) return copy.image
  if (name.includes('document') || name.includes('file') || name.includes('pdf')) return copy.document
  if (name.includes('web') || name.includes('browser') || name.includes('search') || name.includes('url') || name.includes('http')) return copy.webSearch
  return copy.other
}

function displayContent(message: Message) {
  if (message.role !== 'user' || !message.attachments?.length) return message.content
  const marker = message.content.indexOf(ATTACHMENT_CONTEXT_MARKER)
  if (marker < 0) return message.content
  const content = message.content.slice(0, marker)
  return content.startsWith(PHOTO_ONLY_CONTEXT) ? '' : content
}

type DisplayablePhoto = MessageAttachment & { src: string; persisted: boolean }

function displayablePhotos(attachments: Message['attachments']): DisplayablePhoto[] {
  if (!Array.isArray(attachments)) return []
  const photos: DisplayablePhoto[] = []
  for (const attachment of attachments) {
    if (!PHOTO_MIME_TYPES.has(attachment.mime_type) || !attachment.filename) continue
    if (attachment.artifact_id) {
      photos.push({ ...attachment, src: appPath(`/api/v1/artifacts/${encodeURIComponent(attachment.artifact_id)}`), persisted: true })
    } else if (attachment.preview_url?.startsWith(`data:${attachment.mime_type};base64,`)) {
      photos.push({ ...attachment, src: attachment.preview_url, persisted: false })
    }
  }
  return photos
}

function PhotoAttachments({ photos }: { photos: DisplayablePhoto[] }) {
  if (!photos.length) return null
  return <div className={`message-attachments${photos.length === 1 ? ' is-single' : ''}`} role="group" aria-label={t('photoAttachments')}>
    {photos.map((photo, index) => {
      const image = <img src={photo.src} alt={photo.filename} loading={photo.persisted ? 'lazy' : 'eager'} decoding="async" draggable={false} />
      return photo.persisted
        ? <a className="message-photo" href={photo.src} target="_blank" rel="noopener noreferrer" aria-label={`${t('openPhoto')}: ${photo.filename}`} key={`${photo.artifact_id}-${index}`}>{image}</a>
        : <span className="message-photo" key={`preview-${photo.filename}-${index}`}>{image}</span>
    })}
  </div>
}

export function StreamingMessage({ parts, text }: { parts: string[], text: string }) {
  if (!parts.length && !text) return null
  return <article className="message-row is-assistant is-entering" aria-live="polite">
    <div className="message-avatar"><BrandMark compact animated /></div>
    <div className="message-content">
      <div className="message-meta"><span>Selu</span><span className="live-label"><i />{t('live')}</span></div>
      {parts.map((part, index) => <div className="message-surface" key={`${index}-${part.slice(0, 20)}`}><Markdown>{part}</Markdown></div>)}
      {text && <div className="message-surface is-streaming"><Markdown>{text}</Markdown><span className="stream-caret" aria-hidden="true" /></div>}
    </div>
  </article>
}

export function ApprovalCard({ approval, busy, onDecision }: { approval: Approval; busy: boolean; onDecision: (approved: boolean) => void }) {
  return <section className="approval-card" aria-labelledby={`approval-${approval.approval_id}`}>
    <div className="approval-icon"><ShieldCheck aria-hidden="true" /></div>
    <div className="approval-copy">
      <span className="eyebrow">{t('approvalNeeded')}</span>
      <h2 id={`approval-${approval.approval_id}`}>{approval.message || approval.tool_name}</h2>
      {approval.message && <p>{approval.tool_name}</p>}
      {approval.arguments !== undefined && <details>
        <summary>{t('approvalDetails')}<ChevronDown aria-hidden="true" /></summary>
        <pre>{JSON.stringify(approval.arguments, null, 2)}</pre>
      </details>}
      <div className="approval-actions">
        <button disabled={busy} onClick={() => onDecision(false)}>{t('deny')}</button>
        <button className="is-primary" disabled={busy} onClick={() => onDecision(true)}>{t('allow')}</button>
      </div>
    </div>
  </section>
}

export function ActivityTrail({ items, active }: { items: string[], active: boolean }) {
  if (!items.length && !active) return null
  return <details className="activity-trail" open={active}>
    <summary>
      <span className="activity-orbit" aria-hidden="true"><Sparkles /></span>
      <span><strong>{active ? t('working') : t('workDone')}</strong><small>{active ? t('followingAlong') : t('stepsAvailable')}</small></span>
      <ChevronDown className="disclosure-chevron" />
    </summary>
    <ol>
      {items.map((item, index) => <li key={`${item}-${index}`} className={active && index === items.length - 1 ? 'is-current' : ''}>
        <span>{index + 1}</span><p>{item}</p>
      </li>)}
      {active && items.length === 0 && <li className="is-current"><span>1</span><p>{t('gettingReady')}</p></li>}
    </ol>
  </details>
}

function ToolMessage({ message }: { message: Message }) {
  return <details className="tool-message">
    <summary><Wrench /><span>{t('technicalDetails')}</span><ChevronDown /></summary>
    <pre>{message.content}</pre>
  </details>
}

const REMARK_PLUGINS = [remarkGfm]

/**
 * Agent replies use GitHub-flavored Markdown: tables, strikethrough, task
 * lists and bare URLs. Tables get a scroll wrapper so a wide forecast or
 * comparison never stretches the message column on a phone.
 */
function Markdown({ children }: { children: string }) {
  return <ReactMarkdown remarkPlugins={REMARK_PLUGINS} components={{
    a: ({ href, children: label }) => <a href={href} target="_blank" rel="noopener noreferrer">{label}</a>,
    table: ({ children: rows }) => <div className="table-scroll"><table>{rows}</table></div>,
  }}>{children}</ReactMarkdown>
}

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false)
  return <button className="message-action" onClick={async () => {
    await navigator.clipboard.writeText(text)
    setCopied(true)
    window.setTimeout(() => setCopied(false), 1400)
  }} aria-label={copied ? t('copied') : t('copy')}>
    {copied ? <Check /> : <Copy />}{copied ? t('copied') : t('copy')}
  </button>
}

function FeedbackButtons({ feedback }: { feedback: TurnFeedback }) {
  const options: { rating: TurnRating; label: string; Icon: typeof ThumbsUp }[] = [
    { rating: 1, label: t('helpful'), Icon: ThumbsUp },
    { rating: -1, label: t('notHelpful'), Icon: ThumbsDown },
  ]
  return <>
    {options.map(({ rating, label, Icon }) => {
      const selected = feedback.rating === rating
      return <button
        key={rating}
        className={`message-action${selected ? ' is-selected' : ''}`}
        aria-pressed={selected}
        aria-label={label}
        disabled={feedback.busy || selected}
        onClick={() => feedback.onRate(rating)}
      ><Icon />{label}</button>
    })}
  </>
}

/// The engine stores a "[calling <tool>]" stand-in for assistant turns that
/// only invoked tools. Recognize the protocol wrapper even when persisted
/// messages omit the optional `tool_calls` metadata.
export function isToolCallPlaceholder(message: Message) {
  return toolActivityNames(message) !== null
}

function formatTime(value: string) {
  const date = new Date(value)
  return Number.isNaN(date.valueOf()) ? '' : new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit' }).format(date)
}
