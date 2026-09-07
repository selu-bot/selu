import { useEffect, useState } from 'react'

export type Theme = 'light' | 'dark'

export function useStoredBoolean(key: string, fallback: boolean) {
  const [value, setValue] = useState(() => localStorage.getItem(key) === null ? fallback : localStorage.getItem(key) === 'true')
  useEffect(() => localStorage.setItem(key, String(value)), [key, value])
  return [value, setValue] as const
}

export function useTheme() {
  const [theme, setTheme] = useState<Theme>(() => {
    const saved = localStorage.getItem('selu.theme')
    if (saved === 'light' || saved === 'dark') return saved
    return matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light'
  })
  useEffect(() => {
    localStorage.setItem('selu.theme', theme)
    document.documentElement.dataset.theme = theme
  }, [theme])
  return [theme, setTheme] as const
}
