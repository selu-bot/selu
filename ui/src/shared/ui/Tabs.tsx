import { useId, useRef, useState, type KeyboardEvent, type ReactNode } from 'react'
import { cx, getNextEnabledIndex } from './utils'

export type TabItem = {
  id: string
  label: ReactNode
  panel: ReactNode
  disabled?: boolean
}

export type TabsProps = {
  label: string
  tabs: readonly TabItem[]
  value?: string
  defaultValue?: string
  onChange?: (id: string) => void
  activation?: 'automatic' | 'manual'
  className?: string
}

export function Tabs({ label, tabs, value, defaultValue, onChange, activation = 'automatic', className }: TabsProps) {
  const fallback = tabs.find((tab) => !tab.disabled)?.id ?? ''
  const [internalValue, setInternalValue] = useState(defaultValue ?? fallback)
  const selectedValue = value ?? internalValue
  const requestedIndex = tabs.findIndex((tab) => tab.id === selectedValue && !tab.disabled)
  const selectedIndex = requestedIndex >= 0 ? requestedIndex : tabs.findIndex((tab) => !tab.disabled)
  const [focusedIndex, setFocusedIndex] = useState(selectedIndex)
  const tabRefs = useRef<Array<HTMLButtonElement | null>>([])
  const baseId = useId()
  const disabled = tabs.map((tab) => Boolean(tab.disabled))

  const select = (index: number) => {
    const tab = tabs[index]
    if (!tab || tab.disabled) return
    if (value === undefined) setInternalValue(tab.id)
    onChange?.(tab.id)
  }
  const focus = (index: number) => {
    if (index < 0) return
    setFocusedIndex(index)
    tabRefs.current[index]?.focus()
    if (activation === 'automatic') select(index)
  }
  const onKeyDown = (event: KeyboardEvent<HTMLButtonElement>) => {
    if (event.key === 'ArrowRight' || event.key === 'ArrowLeft') {
      event.preventDefault()
      focus(getNextEnabledIndex(focusedIndex, event.key === 'ArrowRight' ? 1 : -1, disabled))
    } else if (event.key === 'Home' || event.key === 'End') {
      event.preventDefault()
      focus(getNextEnabledIndex(event.key === 'Home' ? -1 : 0, event.key === 'Home' ? 1 : -1, disabled))
    } else if ((event.key === 'Enter' || event.key === ' ') && activation === 'manual') {
      event.preventDefault()
      select(focusedIndex)
    }
  }

  if (tabs.length === 0) return null
  const panel = tabs[selectedIndex]
  return <div className={cx('selu-ui-tabs', className)}>
    <div role="tablist" aria-label={label} className="selu-ui-tab-list">
      {tabs.map((tab, index) => <button
        key={tab.id}
        ref={(node) => { tabRefs.current[index] = node }}
        type="button"
        role="tab"
        id={`${baseId}-tab-${index}`}
        aria-controls={`${baseId}-panel-${index}`}
        aria-selected={index === selectedIndex}
        tabIndex={index === selectedIndex ? 0 : -1}
        disabled={tab.disabled}
        onFocus={() => setFocusedIndex(index)}
        onClick={() => select(index)}
        onKeyDown={onKeyDown}
      >{tab.label}</button>)}
    </div>
    <div
      role="tabpanel"
      id={`${baseId}-panel-${selectedIndex}`}
      aria-labelledby={`${baseId}-tab-${selectedIndex}`}
      tabIndex={0}
      className="selu-ui-tab-panel"
    >{panel?.panel}</div>
  </div>
}
