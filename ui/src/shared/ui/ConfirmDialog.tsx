import type { ReactNode } from 'react'
import { Button } from './Button'
import { Dialog } from './Dialog'

export type ConfirmDialogProps = {
  open: boolean
  title: ReactNode
  message: ReactNode
  confirmLabel: ReactNode
  cancelLabel: ReactNode
  onConfirm: () => void
  onCancel: () => void
  busy?: boolean
  destructive?: boolean
  children?: ReactNode
}

export function ConfirmDialog({
  open, title, message, confirmLabel, cancelLabel, onConfirm, onCancel,
  busy = false, destructive = false, children,
}: ConfirmDialogProps) {
  return <Dialog
    open={open}
    onClose={onCancel}
    title={title}
    description={message}
    closeOnBackdrop={!busy}
    closeOnEscape={!busy}
    actions={<>
      <Button variant="secondary" onClick={onCancel} disabled={busy}>{cancelLabel}</Button>
      <Button variant={destructive ? 'danger' : 'primary'} onClick={onConfirm} loading={busy}>{confirmLabel}</Button>
    </>}
  >
    {children}
  </Dialog>
}
