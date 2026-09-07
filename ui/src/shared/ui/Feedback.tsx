import type { HTMLAttributes, ReactNode } from 'react'
import { cx } from './utils'

export type StatusBadgeProps = HTMLAttributes<HTMLSpanElement> & {
  tone?: 'neutral' | 'success' | 'warning' | 'danger' | 'info'
  dot?: boolean
  children: ReactNode
}

export function StatusBadge({ className, tone = 'neutral', dot = true, children, ...props }: StatusBadgeProps) {
  return <span {...props} className={cx('selu-ui-status', `is-${tone}`, className)}>
    {dot && <span className="selu-ui-status-dot" aria-hidden="true" />}{children}
  </span>
}

export type SkeletonProps = HTMLAttributes<HTMLSpanElement> & {
  width?: string | number
  height?: string | number
  radius?: string | number
  label?: string
}

export function Skeleton({ className, width, height, radius, label, style, ...props }: SkeletonProps) {
  return <span
    {...props}
    role={label ? 'status' : undefined}
    aria-label={label}
    aria-hidden={label ? undefined : true}
    className={cx('selu-ui-skeleton', className)}
    style={{ width, height, borderRadius: radius, ...style }}
  />
}
