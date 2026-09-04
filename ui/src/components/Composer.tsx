import { ArrowUp, CornerDownLeft, LockKeyhole } from 'lucide-react'
import { FormEvent, KeyboardEvent, useLayoutEffect, useRef } from 'react'
import { t } from '../i18n'

type ComposerProps = {
  value: string
  disabled: boolean
  busy: boolean
  onChange: (value: string) => void
  onSend: () => void
}

export function Composer({ value, disabled, busy, onChange, onSend }: ComposerProps) {
  const field = useRef<HTMLTextAreaElement>(null)
  // Grow with the draft in browsers without `field-sizing: content` (Safari).
  // The CSS max-height caps it, after which the textarea scrolls.
  useLayoutEffect(() => {
    const node = field.current
    if (!node) return
    node.style.height = '38px'
    node.style.height = `${Math.max(38, node.scrollHeight)}px`
  }, [value])
  const submit = (event?: FormEvent) => {
    event?.preventDefault()
    if (!disabled && value.trim()) onSend()
  }
  const keyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault()
      submit()
    }
  }

  return <div className="composer-wrap">
    <form className="composer" onSubmit={submit}>
      <textarea
        ref={field}
        rows={1}
        value={value}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={keyDown}
        placeholder={busy ? t('waitForReply') : t('placeholder')}
        aria-label={t('placeholder')}
        disabled={disabled}
      />
      <button className="send-button" disabled={disabled || !value.trim()} aria-label={t('send')}>
        <ArrowUp />
      </button>
      <div className="composer-glow" aria-hidden="true" />
    </form>
    <div className="composer-note">
      <span><CornerDownLeft />{t('sendHint')}</span>
      <span><LockKeyhole />{t('privateByDesign')}</span>
    </div>
  </div>
}
