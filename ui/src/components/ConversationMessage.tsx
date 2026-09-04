import { Check, ChevronDown, Copy, ShieldCheck, Sparkles, ThumbsDown, ThumbsUp, Wrench } from 'lucide-react'
import { useState } from 'react'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import type { Approval, Message, TurnRating } from '../api'
import { t } from '../i18n'
import { BrandMark } from './BrandMark'

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
  if (message.role === 'tool') return <ToolMessage message={message} />
  if (message.role === 'system') return null
  if (message.role === 'assistant' && isToolCallPlaceholder(message)) return null
  return <article className={`message-row is-${message.role}${entering ? ' is-entering' : ''}`}>
    {message.role === 'assistant' && <div className="message-avatar"><BrandMark compact /></div>}
    <div className="message-content">
      <div className="message-meta">
        <span>{message.role === 'user' ? t('you') : 'Selu'}</span>
        <time dateTime={message.created_at}>{formatTime(message.created_at)}</time>
      </div>
      <div className="message-surface">
        <Markdown>{message.content}</Markdown>
      </div>
      {message.role === 'assistant' && <div className="message-actions">
        <CopyButton text={message.content} />
        {feedback && <FeedbackButtons feedback={feedback} />}
      </div>}
    </div>
  </article>
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
/// only invoked tools. The following tool message already exposes the details.
function isToolCallPlaceholder(message: Message) {
  const hasToolCalls = Array.isArray(message.tool_calls) && message.tool_calls.length > 0
  return hasToolCalls && /^\s*(\[calling [^\]]*\]\s*)+$/.test(message.content)
}

function formatTime(value: string) {
  const date = new Date(value)
  return Number.isNaN(date.valueOf()) ? '' : new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit' }).format(date)
}
