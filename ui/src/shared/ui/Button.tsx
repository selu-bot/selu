import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from 'react'
import { cx } from './utils'

export type ButtonProps = ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: 'primary' | 'secondary' | 'ghost' | 'danger'
  size?: 'sm' | 'md' | 'lg'
  loading?: boolean
  leadingIcon?: ReactNode
  trailingIcon?: ReactNode
}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
  { className, variant = 'secondary', size = 'md', loading = false, disabled, leadingIcon, trailingIcon, children, type = 'button', ...props },
  ref,
) {
  return <button
    {...props}
    ref={ref}
    type={type}
    disabled={disabled || loading}
    aria-busy={loading || undefined}
    className={cx('selu-ui-button', `is-${variant}`, `is-${size}`, className)}
  >
    {leadingIcon && <span className="selu-ui-button-icon" aria-hidden="true">{leadingIcon}</span>}
    <span>{children}</span>
    {trailingIcon && <span className="selu-ui-button-icon" aria-hidden="true">{trailingIcon}</span>}
  </button>
})

export type IconButtonProps = Omit<ButtonHTMLAttributes<HTMLButtonElement>, 'aria-label'> & {
  label: string
  size?: 'sm' | 'md' | 'lg'
  variant?: 'ghost' | 'secondary' | 'danger'
}

export const IconButton = forwardRef<HTMLButtonElement, IconButtonProps>(function IconButton(
  { className, label, size = 'md', variant = 'ghost', children, type = 'button', ...props },
  ref,
) {
  return <button
    {...props}
    ref={ref}
    type={type}
    aria-label={label}
    className={cx('selu-ui-icon-button', `is-${size}`, `is-${variant}`, className)}
  >
    <span aria-hidden="true">{children}</span>
  </button>
})
