import type { ReactNode } from 'react'
import { X } from 'lucide-react'
import { Card, Dialog, IconButton, Skeleton } from '../../shared/ui'
import './management.css'

export function OverviewGrid({ children }: { children: ReactNode }) {
  return <div className="management-grid">{children}</div>
}

export function OverviewCard({ icon, status, title, description, meta, actions, children }: {
  icon: ReactNode
  status?: ReactNode
  title: ReactNode
  description?: ReactNode
  meta?: ReactNode
  actions?: ReactNode
  children?: ReactNode
}) {
  return <Card className="management-card" variant="interactive" padding="md">
    <div className="management-card-top"><span className="management-card-icon" aria-hidden="true">{icon}</span>{status}</div>
    <h2>{title}</h2>
    {description && <div className="management-card-description">{description}</div>}
    {children}
    {(meta || actions) && <footer><div>{meta}</div>{actions && <div className="management-card-actions">{actions}</div>}</footer>}
  </Card>
}

export function ManagementSheet({ open, title, description, onClose, closeLabel, actions, children, busy = false }: {
  open: boolean
  title: ReactNode
  description?: ReactNode
  onClose: () => void
  closeLabel: string
  actions?: ReactNode
  children: ReactNode
  busy?: boolean
}) {
  return <Dialog
    open={open}
    onClose={onClose}
    title={title}
    headerAction={<IconButton label={closeLabel} onClick={onClose} disabled={busy}><X /></IconButton>}
    description={description}
    actions={actions}
    closeOnBackdrop={!busy}
    closeOnEscape={!busy}
    className="management-sheet"
    backdropClassName="management-sheet-backdrop"
  >{children}</Dialog>
}

export function ManagementLoading({ cards = 3 }: { cards?: number }) {
  return <div className="management-grid" aria-busy="true">{Array.from({ length: cards }, (_, index) => <Card key={index} padding="md" className="management-card"><Skeleton width={42} height={42} radius={14} /><Skeleton width="58%" height={18} /><Skeleton width="88%" height={12} /><Skeleton width="70%" height={12} /></Card>)}</div>
}

export function ManagementSection({ title, description, actions, children }: { title: ReactNode; description?: ReactNode; actions?: ReactNode; children: ReactNode }) {
  return <section className="management-section"><header><div><h2>{title}</h2>{description && <p>{description}</p>}</div>{actions}</header><div className="management-section-body">{children}</div></section>
}
