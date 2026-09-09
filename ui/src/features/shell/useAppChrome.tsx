import { useEffect, useState } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { useNavigate } from '@tanstack/react-router'
import { api } from '../../api'
import { AppNavigation } from '../../components/AppNavigation'
import { getLanguage, setLanguage, type Language } from '../../i18n'
import { useStoredBoolean, useTheme } from '../../shared/preferences'

export type AppSection = 'home' | 'saved' | 'past' | 'conversations' | 'automations' | 'agents' | 'connectors' | 'updates' | 'connections' | 'about-you' | 'people' | 'feedback' | 'settings'

export function useAppChrome(active: AppSection) {
  const cache = useQueryClient()
  const navigate = useNavigate()
  const [mobileNavigation, setMobileNavigation] = useState(false)
  const [navCollapsed, setNavCollapsed] = useStoredBoolean('selu.navigation.collapsed', false)
  const [theme, setTheme] = useTheme()
  const [language, setLanguageState] = useState<Language>(getLanguage)
  const session = useQuery({ queryKey: ['session'], queryFn: api.session, staleTime: Infinity })
  useEffect(() => {
    if (localStorage.getItem('selu.language') || !session.data) return
    const preferred: Language = session.data.language.toLowerCase().startsWith('de') ? 'de' : 'en'
    setLanguage(preferred)
    setLanguageState(preferred)
  }, [session.data])
  const logout = async () => {
    await api.logout()
    cache.clear()
    await navigate({ to: '/app/login', replace: true })
  }

  const navigation = <AppNavigation
    active={active}
    collapsed={navCollapsed}
    mobileOpen={mobileNavigation}
    theme={theme}
    isAdmin={session.data?.is_admin ?? false}
    onCollapse={() => setNavCollapsed(!navCollapsed)}
    onCloseMobile={() => setMobileNavigation(false)}
    onLanguage={() => {
      const next = language === 'de' ? 'en' : 'de'
      setLanguage(next)
      setLanguageState(next)
    }}
    onTheme={() => setTheme(theme === 'dark' ? 'light' : 'dark')}
    onLogout={() => void logout()}
  />

  return { navigation, navCollapsed, language, openMobileNavigation: () => setMobileNavigation(true), session }
}
