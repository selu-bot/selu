import { ArrowUp, CornerDownLeft, LockKeyhole, Slash } from 'lucide-react'
import { FormEvent, KeyboardEvent, useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { SlashCommand } from '../api'
import { t } from '../i18n'

type ComposerProps = {
  value: string
  disabled: boolean
  busy: boolean
  commands: SlashCommand[]
  onChange: (value: string) => void
  onSend: () => void
}

export function Composer({ value, disabled, busy, commands, onChange, onSend }: ComposerProps) {
  const field = useRef<HTMLTextAreaElement>(null)
  const [highlight, setHighlight] = useState(0)
  const [dismissed, setDismissed] = useState(false)
  // Grow with the draft in browsers without `field-sizing: content` (Safari).
  // The CSS max-height caps it, after which the textarea scrolls.
  useLayoutEffect(() => {
    const node = field.current
    if (!node) return
    node.style.height = '38px'
    node.style.height = `${Math.max(38, node.scrollHeight)}px`
  }, [value])

  const suggestions = dismissed ? [] : matchCommands(commands, value)
  const hinted = argumentHint(commands, value)
  useEffect(() => { setHighlight(0); setDismissed(false) }, [value])

  const choose = (command: SlashCommand) => {
    onChange(command.command)
    field.current?.focus()
  }
  const submit = (event?: FormEvent) => {
    event?.preventDefault()
    if (!disabled && value.trim()) onSend()
  }
  const keyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (suggestions.length) {
      if (event.key === 'ArrowDown') { event.preventDefault(); setHighlight((highlight + 1) % suggestions.length); return }
      if (event.key === 'ArrowUp') { event.preventDefault(); setHighlight((highlight + suggestions.length - 1) % suggestions.length); return }
      if (event.key === 'Enter' || event.key === 'Tab') { event.preventDefault(); choose(suggestions[highlight]); return }
      if (event.key === 'Escape') { event.preventDefault(); setDismissed(true); return }
    }
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault()
      submit()
    }
  }
  const menuOpen = suggestions.length > 0

  return <div className="composer-wrap">
    <form className="composer" onSubmit={submit}>
      {commands.length > 0 && <button
        type="button"
        className="composer-tool"
        aria-label={t('commands')}
        title={t('commands')}
        disabled={disabled}
        onClick={() => { onChange('/'); field.current?.focus() }}
      ><Slash /></button>}
      <textarea
        ref={field}
        rows={1}
        value={value}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={keyDown}
        placeholder={busy ? t('waitForReply') : t('placeholder')}
        aria-label={t('placeholder')}
        aria-autocomplete="list"
        aria-controls="command-menu"
        aria-expanded={menuOpen}
        aria-activedescendant={menuOpen ? `command-option-${highlight}` : undefined}
        disabled={disabled}
      />
      <button className="send-button" disabled={disabled || !value.trim()} aria-label={t('send')}>
        <ArrowUp />
      </button>
      <div className="composer-glow" aria-hidden="true" />
      {menuOpen && <div className="command-menu" role="listbox" id="command-menu" aria-label={t('commands')}>
        {suggestions.map((command, index) => <button
          type="button"
          role="option"
          key={command.label}
          id={`command-option-${index}`}
          aria-selected={index === highlight}
          onMouseDown={(event) => event.preventDefault()}
          onMouseEnter={() => setHighlight(index)}
          onClick={() => choose(command)}
        >
          <code>{command.label}{command.argument_hint && <em> &lt;{command.argument_hint}&gt;</em>}</code>
          <span>{command.description}</span>
        </button>)}
      </div>}
      {!menuOpen && hinted && <div className="command-hint" aria-live="polite">
        <code>{hinted.label}</code><span>&lt;{hinted.argument_hint}&gt;</span><span>· {hinted.description}</span>
      </div>}
    </form>
    <div className="composer-note">
      <span><CornerDownLeft />{t('sendHint')}</span>
      {commands.length > 0 && <span><Slash />{t('commandsHint')}</span>}
      <span><LockKeyhole />{t('privateByDesign')}</span>
    </div>
  </div>
}

/** Commands whose label extends what the user has typed so far. */
function matchCommands(commands: SlashCommand[], value: string) {
  if (!value.startsWith('/') || value.includes('\n')) return []
  const typed = value.trimEnd().toLowerCase()
  return commands.filter((command) => {
    const label = command.label.toLowerCase()
    return label.startsWith(typed) && label !== typed && command.command.toLowerCase() !== value.toLowerCase()
  })
}

/** The command the draft has completed, when it still expects an argument. */
function argumentHint(commands: SlashCommand[], value: string) {
  const lower = value.toLowerCase()
  return commands.find((command) => command.argument_hint
    && (lower.startsWith(command.command.toLowerCase()) || lower.trimEnd() === command.label.toLowerCase())) ?? null
}
