import {
  useEffect, useId, useRef, useState, type KeyboardEvent, type ReactNode,
} from 'react'
import { cx, getNextEnabledIndex } from './utils'

export type MenuItem = {
  id: string
  label: ReactNode
  textValue: string
  icon?: ReactNode
  disabled?: boolean
  danger?: boolean
  onSelect: () => void
}

export type MenuProps = {
  label: string
  trigger: ReactNode
  items: readonly MenuItem[]
  align?: 'start' | 'end'
  className?: string
}

export function Menu({ label, trigger, items, align = 'end', className }: MenuProps) {
  const [open, setOpen] = useState(false)
  const [activeIndex, setActiveIndex] = useState(-1)
  const rootRef = useRef<HTMLDivElement>(null)
  const triggerRef = useRef<HTMLButtonElement>(null)
  const itemRefs = useRef<Array<HTMLButtonElement | null>>([])
  const menuId = useId()
  const disabled = items.map((item) => Boolean(item.disabled))

  const focusItem = (index: number) => {
    if (index < 0) return
    setActiveIndex(index)
    requestAnimationFrame(() => itemRefs.current[index]?.focus())
  }
  const openAt = (index: number) => {
    setOpen(true)
    focusItem(index)
  }
  const close = (restoreFocus = true) => {
    setOpen(false)
    setActiveIndex(-1)
    if (restoreFocus) requestAnimationFrame(() => triggerRef.current?.focus())
  }

  useEffect(() => {
    if (!open) return
    const onPointerDown = (event: PointerEvent) => {
      if (!rootRef.current?.contains(event.target as Node)) close(false)
    }
    document.addEventListener('pointerdown', onPointerDown)
    return () => document.removeEventListener('pointerdown', onPointerDown)
  }, [open])

  const onTriggerKeyDown = (event: KeyboardEvent<HTMLButtonElement>) => {
    if (event.key === 'ArrowDown' || event.key === 'Enter' || event.key === ' ') {
      event.preventDefault()
      openAt(getNextEnabledIndex(-1, 1, disabled))
    } else if (event.key === 'ArrowUp') {
      event.preventDefault()
      openAt(getNextEnabledIndex(0, -1, disabled))
    }
  }

  const onMenuKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.key === 'Escape') { event.preventDefault(); close(); return }
    if (event.key === 'Tab') { close(false); return }
    if (event.key === 'Home' || event.key === 'End') {
      event.preventDefault()
      focusItem(getNextEnabledIndex(event.key === 'Home' ? -1 : 0, event.key === 'Home' ? 1 : -1, disabled))
      return
    }
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault()
      focusItem(getNextEnabledIndex(activeIndex, event.key === 'ArrowDown' ? 1 : -1, disabled))
      return
    }
    if (event.key.length === 1 && !event.metaKey && !event.ctrlKey && !event.altKey) {
      const query = event.key.toLocaleLowerCase()
      const match = items.findIndex((item, index) => !disabled[index] && item.textValue.toLocaleLowerCase().startsWith(query))
      if (match >= 0) focusItem(match)
    }
  }

  return <div ref={rootRef} className={cx('selu-ui-menu', className)}>
    <button
      ref={triggerRef}
      type="button"
      className="selu-ui-menu-trigger"
      aria-label={label}
      aria-haspopup="menu"
      aria-expanded={open}
      aria-controls={open ? menuId : undefined}
      onClick={() => open ? close(false) : openAt(getNextEnabledIndex(-1, 1, disabled))}
      onKeyDown={onTriggerKeyDown}
    >{trigger}</button>
    {open && <div id={menuId} role="menu" className={cx('selu-ui-menu-popover', `is-${align}`)} onKeyDown={onMenuKeyDown}>
      {items.map((item, index) => <button
        key={item.id}
        ref={(node) => { itemRefs.current[index] = node }}
        type="button"
        role="menuitem"
        tabIndex={activeIndex === index ? 0 : -1}
        disabled={item.disabled}
        className={cx(item.danger && 'is-danger')}
        onMouseEnter={() => !item.disabled && setActiveIndex(index)}
        onClick={() => { if (!item.disabled) { close(); item.onSelect() } }}
      >
        {item.icon && <span className="selu-ui-menu-icon" aria-hidden="true">{item.icon}</span>}
        <span>{item.label}</span>
      </button>)}
    </div>}
  </div>
}
