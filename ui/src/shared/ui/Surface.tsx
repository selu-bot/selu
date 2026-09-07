import type { HTMLAttributes, ReactNode } from 'react'
import { cx } from './utils'

export type CardProps = HTMLAttributes<HTMLDivElement> & {
  variant?: 'default' | 'elevated' | 'interactive'
  padding?: 'none' | 'sm' | 'md' | 'lg'
}

export function Card({ className, variant = 'default', padding = 'md', ...props }: CardProps) {
  return <div {...props} className={cx('selu-ui-card', `is-${variant}`, `has-${padding}-padding`, className)} />
}

export type PageHeaderProps = HTMLAttributes<HTMLElement> & {
  eyebrow?: ReactNode
  title: ReactNode
  description?: ReactNode
  leading?: ReactNode
  actions?: ReactNode
}

export function PageHeader({ className, eyebrow, title, description, leading, actions, ...props }: PageHeaderProps) {
  return <header {...props} className={cx('selu-ui-page-header', className)}>
    {leading && <div className="selu-ui-page-header-leading">{leading}</div>}
    <div className="selu-ui-page-header-copy">
      {eyebrow && <div className="selu-ui-eyebrow">{eyebrow}</div>}
      <h1>{title}</h1>
      {description && <div className="selu-ui-page-header-description">{description}</div>}
    </div>
    {actions && <div className="selu-ui-page-header-actions">{actions}</div>}
  </header>
}

export type EmptyStateProps = HTMLAttributes<HTMLDivElement> & {
  icon?: ReactNode
  title: ReactNode
  description?: ReactNode
  action?: ReactNode
}

export function EmptyState({ className, icon, title, description, action, ...props }: EmptyStateProps) {
  return <div {...props} className={cx('selu-ui-empty-state', className)}>
    {icon && <div className="selu-ui-empty-state-icon" aria-hidden="true">{icon}</div>}
    <h2>{title}</h2>
    {description && <div className="selu-ui-empty-state-description">{description}</div>}
    {action && <div className="selu-ui-empty-state-action">{action}</div>}
  </div>
}
