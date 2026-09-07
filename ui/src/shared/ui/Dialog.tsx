import {
  useEffect, useId, useRef, type MouseEvent as ReactMouseEvent, type ReactNode, type RefObject,
} from 'react'
import { createPortal } from 'react-dom'
import { cx, focusableSelector } from './utils'

export type DialogProps = {
  open: boolean
  onClose: () => void
  title: ReactNode
  children: ReactNode
  description?: ReactNode
  actions?: ReactNode
  headerAction?: ReactNode
  className?: string
  backdropClassName?: string
  initialFocusRef?: RefObject<HTMLElement | null>
  closeOnEscape?: boolean
  closeOnBackdrop?: boolean
}

export function Dialog({
  open, onClose, title, description, children, actions, headerAction, className, backdropClassName,
  initialFocusRef, closeOnEscape = true, closeOnBackdrop = true,
}: DialogProps) {
  const titleId = useId()
  const descriptionId = useId()
  const dialogRef = useRef<HTMLDivElement>(null)
  const closeRef = useRef(onClose)
  closeRef.current = onClose

  useEffect(() => {
    if (!open) return
    const previouslyFocused = document.activeElement instanceof HTMLElement ? document.activeElement : null
    const previousOverflow = document.body.style.overflow
    document.body.style.overflow = 'hidden'
    const dialog = dialogRef.current
    const focusTarget = initialFocusRef?.current ?? dialog?.querySelector<HTMLElement>(focusableSelector) ?? dialog
    focusTarget?.focus()

    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape' && closeOnEscape) {
        event.preventDefault()
        closeRef.current()
        return
      }
      if (event.key !== 'Tab' || !dialog) return
      const focusable = Array.from(dialog.querySelectorAll<HTMLElement>(focusableSelector))
        .filter((element) => !element.hidden && element.getAttribute('aria-hidden') !== 'true')
      if (focusable.length === 0) {
        event.preventDefault()
        dialog.focus()
        return
      }
      const first = focusable[0]
      const last = focusable[focusable.length - 1]
      if (event.shiftKey && (document.activeElement === first || document.activeElement === dialog)) {
        event.preventDefault()
        last.focus()
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault()
        first.focus()
      }
    }
    document.addEventListener('keydown', onKeyDown)
    return () => {
      document.removeEventListener('keydown', onKeyDown)
      document.body.style.overflow = previousOverflow
      if (previouslyFocused?.isConnected) previouslyFocused.focus()
    }
  }, [open, initialFocusRef, closeOnEscape])

  if (!open) return null
  const onBackdrop = (event: ReactMouseEvent<HTMLDivElement>) => {
    if (closeOnBackdrop && event.target === event.currentTarget) onClose()
  }
  return createPortal(
    <div className={cx('selu-ui-dialog-backdrop', backdropClassName)} onMouseDown={onBackdrop}>
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={description ? descriptionId : undefined}
        tabIndex={-1}
        className={cx('selu-ui-dialog', className)}
      >
        <div className="selu-ui-dialog-heading">
          <div className="selu-ui-dialog-title-row">
            <h2 id={titleId}>{title}</h2>
            {headerAction}
          </div>
          {description && <div id={descriptionId} className="selu-ui-dialog-description">{description}</div>}
        </div>
        <div className="selu-ui-dialog-body">{children}</div>
        {actions && <div className="selu-ui-dialog-actions">{actions}</div>}
      </div>
    </div>,
    document.body,
  )
}
