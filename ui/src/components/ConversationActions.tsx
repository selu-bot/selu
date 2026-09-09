import { Bookmark, MoreHorizontal, Pencil, Trash2 } from 'lucide-react'
import { FormEvent, useEffect, useId, useRef, useState } from 'react'
import { t } from '../i18n'

type ConversationMenuProps = {
  onRename: () => void
  onDelete: () => void
}

export function ConversationMenu({ onRename, onDelete }: ConversationMenuProps) {
  const [open, setOpen] = useState(false)
  const root = useRef<HTMLDivElement>(null)
  const menuId = useId()

  useEffect(() => {
    if (!open) return
    const close = (event: MouseEvent | KeyboardEvent) => {
      if (event instanceof KeyboardEvent && event.key !== 'Escape') return
      if (event instanceof MouseEvent && root.current?.contains(event.target as Node)) return
      setOpen(false)
    }
    document.addEventListener('mousedown', close)
    document.addEventListener('keydown', close)
    return () => {
      document.removeEventListener('mousedown', close)
      document.removeEventListener('keydown', close)
    }
  }, [open])

  return <div className="conversation-menu" ref={root}>
    <button
      className="icon-button"
      aria-label={t('conversationOptions')}
      aria-haspopup="menu"
      aria-expanded={open}
      aria-controls={menuId}
      onClick={() => setOpen(!open)}
    ><MoreHorizontal /></button>
    {open && <div className="menu-popover" role="menu" id={menuId}>
      <button role="menuitem" onClick={() => { setOpen(false); onRename() }}><Pencil />{t('rename')}</button>
      <button role="menuitem" className="is-danger" onClick={() => { setOpen(false); onDelete() }}><Trash2 />{t('deleteConversation')}</button>
    </div>}
  </div>
}

type RenameProps = {
  initialTitle: string
  busy: boolean
  error?: string | null
  onCancel: () => void
  onSave: (title: string) => void
}

export function RenameConversationDialog({ initialTitle, busy, error, onCancel, onSave }: RenameProps) {
  const [title, setTitle] = useState(initialTitle)
  const headingId = useId()
  const submit = (event: FormEvent) => {
    event.preventDefault()
    const next = title.trim()
    if (next && !busy) onSave(next)
  }
  return <Modal labelledBy={headingId} onClose={onCancel}>
    <form className="dialog-card" onSubmit={submit}>
      <h2 id={headingId}>{t('renameConversation')}</h2>
      <input
        autoFocus
        value={title}
        maxLength={120}
        placeholder={t('titlePlaceholder')}
        aria-label={t('renameConversation')}
        onChange={(event) => setTitle(event.target.value)}
        onFocus={(event) => event.target.select()}
      />
      {error && <p className="dialog-error" role="alert">{error}</p>}
      <div className="dialog-actions">
        <button type="button" onClick={onCancel} disabled={busy}>{t('cancel')}</button>
        <button type="submit" className="is-primary" disabled={busy || !title.trim()}>{t('save')}</button>
      </div>
    </form>
  </Modal>
}

type SaveTopicProps = {
  initialTitle: string
  busy: boolean
  onCancel: () => void
  onSave: (title: string) => void
}

export function SaveTopicDialog({ initialTitle, busy, onCancel, onSave }: SaveTopicProps) {
  const [title, setTitle] = useState(initialTitle)
  const headingId = useId()
  const submit = (event: FormEvent) => {
    event.preventDefault()
    const next = title.trim()
    if (next && !busy) onSave(next)
  }
  return <Modal labelledBy={headingId} onClose={onCancel}>
    <form className="dialog-card" onSubmit={submit}>
      <span className="dialog-symbol" aria-hidden="true"><Bookmark /></span>
      <h2 id={headingId}>{t('nameTopic')}</h2>
      <p>{t('nameTopicHint')}</p>
      <input
        autoFocus
        value={title}
        maxLength={120}
        placeholder={t('titlePlaceholder')}
        aria-label={t('nameTopic')}
        onChange={(event) => setTitle(event.target.value)}
        onFocus={(event) => event.target.select()}
      />
      <div className="dialog-actions">
        <button type="button" onClick={onCancel} disabled={busy}>{t('cancel')}</button>
        <button type="submit" className="is-primary" disabled={busy || !title.trim()}>{t('saveTopic')}</button>
      </div>
    </form>
  </Modal>
}

type DeleteProps = {
  title: string
  blocked: boolean
  busy: boolean
  error?: string | null
  onCancel: () => void
  onConfirm: () => void
}

export function DeleteConversationDialog({ title, blocked, busy, error, onCancel, onConfirm }: DeleteProps) {
  const headingId = useId()
  return <Modal labelledBy={headingId} onClose={onCancel}>
    <div className="dialog-card">
      <h2 id={headingId}>{t('deleteConversation')}</h2>
      <p className="dialog-subject">{title}</p>
      <p>{blocked ? t('deleteWhileWorking') : t('deleteConversationBody')}</p>
      {error && <p className="dialog-error" role="alert">{error}</p>}
      <div className="dialog-actions">
        <button type="button" onClick={onCancel} disabled={busy}>{t('cancel')}</button>
        <button type="button" className="is-danger" disabled={busy || blocked} onClick={onConfirm} autoFocus={!blocked}>
          {t('deleteConversation')}
        </button>
      </div>
    </div>
  </Modal>
}

function Modal({ labelledBy, onClose, children }: { labelledBy: string; onClose: () => void; children: React.ReactNode }) {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => { if (event.key === 'Escape') onClose() }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [onClose])
  return <div className="dialog-scrim" onMouseDown={(event) => { if (event.target === event.currentTarget) onClose() }}>
    <div role="dialog" aria-modal="true" aria-labelledby={labelledBy} className="dialog">
      {children}
    </div>
  </div>
}
