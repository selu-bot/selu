import { ArrowUp, CornerDownLeft, ImagePlus, LockKeyhole, Slash, X } from 'lucide-react'
import { FormEvent, KeyboardEvent, useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { SlashCommand } from '../api'
import { t } from '../i18n'
import { PHOTO_ACCEPT, type SelectedPhoto } from '../shared/photoUploads'

type ComposerProps = {
  value: string
  disabled: boolean
  busy: boolean
  commands: SlashCommand[]
  photos: SelectedPhoto[]
  supportsPhotoUploads: boolean
  photoSelectionBusy?: boolean
  onChange: (value: string) => void
  onAddPhotos: (files: File[]) => void
  onRemovePhoto: (id: string) => void
  onSend: () => void
}

export function Composer({ value, disabled, busy, commands, photos, supportsPhotoUploads, photoSelectionBusy = false, onChange, onAddPhotos, onRemovePhoto, onSend }: ComposerProps) {
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
    if (!disabled && (value.trim() || photos.length > 0)) onSend()
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
  const canSend = Boolean(value.trim() || photos.length)

  return <div className="composer-wrap">
    <form className="composer" onSubmit={submit}>
      <PhotoPreviewStrip photos={photos} disabled={disabled} onRemove={onRemovePhoto} />
      {supportsPhotoUploads && <PhotoPickerButton
        className="composer-tool photo-picker-button"
        disabled={disabled || photoSelectionBusy}
        onSelect={onAddPhotos}
      />}
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
      <button className="send-button" disabled={disabled || !canSend} aria-label={t('send')}>
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

export function PhotoPickerButton({ className, disabled, onSelect }: { className: string; disabled: boolean; onSelect: (files: File[]) => void }) {
  const input = useRef<HTMLInputElement>(null)
  return <>
    <input
      ref={input}
      className="photo-input"
      type="file"
      accept={PHOTO_ACCEPT}
      multiple
      disabled={disabled}
      onChange={(event) => {
        const files = Array.from(event.currentTarget.files ?? [])
        event.currentTarget.value = ''
        if (files.length) onSelect(files)
      }}
    />
    <button
      type="button"
      className={className}
      aria-label={t('addPhotos')}
      title={t('addPhotos')}
      disabled={disabled}
      onClick={() => input.current?.click()}
    ><ImagePlus /></button>
  </>
}

export function PhotoPreviewStrip({ photos, disabled, onRemove }: { photos: SelectedPhoto[]; disabled: boolean; onRemove: (id: string) => void }) {
  if (!photos.length) return null
  return <div className="photo-preview-strip" role="list" aria-label={t('selectedPhotos')}>
    {photos.map((photo) => <div className="photo-preview" role="listitem" key={photo.id}>
      <img src={photo.preview_url} alt={photo.filename} />
      <button
        type="button"
        className="photo-preview-remove"
        aria-label={`${t('removePhoto')}: ${photo.filename}`}
        title={t('removePhoto')}
        disabled={disabled}
        onClick={() => onRemove(photo.id)}
      ><X /></button>
    </div>)}
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
