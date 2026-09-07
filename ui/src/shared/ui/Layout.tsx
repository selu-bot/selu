import type { HTMLAttributes, ReactNode } from 'react'
import { cx } from './utils'

export type PageLayoutProps = HTMLAttributes<HTMLDivElement> & {
  header?: ReactNode
  sidebar?: ReactNode
  aside?: ReactNode
  children: ReactNode
  width?: 'narrow' | 'default' | 'wide' | 'full'
}

export function PageLayout({ className, header, sidebar, aside, children, width = 'default', ...props }: PageLayoutProps) {
  return <div {...props} className={cx('selu-ui-page-layout', `is-${width}`, Boolean(sidebar) && 'has-sidebar', Boolean(aside) && 'has-aside', className)}>
    {header && <div className="selu-ui-page-layout-header">{header}</div>}
    <div className="selu-ui-page-layout-grid">
      {sidebar && <aside className="selu-ui-page-layout-sidebar">{sidebar}</aside>}
      <main className="selu-ui-page-layout-main">{children}</main>
      {aside && <aside className="selu-ui-page-layout-aside">{aside}</aside>}
    </div>
  </div>
}

export type DataListProps = HTMLAttributes<HTMLUListElement> & {
  children: ReactNode
  divided?: boolean
}

export function DataList({ className, children, divided = true, ...props }: DataListProps) {
  return <ul {...props} className={cx('selu-ui-data-list', divided && 'is-divided', className)}>{children}</ul>
}

export type DataListItemProps = HTMLAttributes<HTMLLIElement> & {
  leading?: ReactNode
  title: ReactNode
  description?: ReactNode
  meta?: ReactNode
  actions?: ReactNode
}

export function DataListItem({ className, leading, title, description, meta, actions, ...props }: DataListItemProps) {
  return <li {...props} className={cx('selu-ui-data-list-item', className)}>
    {leading && <div className="selu-ui-data-list-leading" aria-hidden="true">{leading}</div>}
    <div className="selu-ui-data-list-copy">
      <div className="selu-ui-data-list-title">{title}</div>
      {description && <div className="selu-ui-data-list-description">{description}</div>}
    </div>
    {meta && <div className="selu-ui-data-list-meta">{meta}</div>}
    {actions && <div className="selu-ui-data-list-actions">{actions}</div>}
  </li>
}
