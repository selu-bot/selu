import {
  Bot, Boxes, CalendarClock, ChevronLeft, Database, Globe2, Heart, KeyRound, LogOut,
  MessageCircle, MoonStar, PanelLeftClose, PanelLeftOpen, Settings2, ShieldCheck,
  Smartphone, Sun, Users, Waypoints, X,
} from 'lucide-react'
import { appPath } from '../api'
import { getLanguage, t, type TranslationKey } from '../i18n'
import { BrandMark } from './BrandMark'

type AppNavigationProps = {
  collapsed: boolean
  mobileOpen: boolean
  theme: 'light' | 'dark'
  isAdmin: boolean
  onCollapse: () => void
  onCloseMobile: () => void
  onLanguage: () => void
  onTheme: () => void
}

const primary = [
  { label: 'chat' as const, href: '/app/', icon: MessageCircle, active: true },
  { label: 'memory' as const, href: '/personality', icon: Database },
  { label: 'schedules' as const, href: '/schedules', icon: CalendarClock },
]

const manage = [
  { label: 'agents' as const, href: '/agents', icon: Bot },
  { label: 'connections' as const, href: '/pipes', icon: Waypoints },
  { label: 'people' as const, href: '/users', icon: Users },
  { label: 'providers' as const, href: '/providers', icon: ShieldCheck },
  { label: 'mobileApp' as const, href: '/mobile', icon: Smartphone },
]

export function AppNavigation(props: AppNavigationProps) {
  const { collapsed, isAdmin, mobileOpen, theme, onCollapse, onCloseMobile, onLanguage, onTheme } = props
  const navClass = `app-navigation${collapsed ? ' is-collapsed' : ''}${mobileOpen ? ' is-mobile-open' : ''}`

  return <>
    {mobileOpen && <button className="nav-scrim" onClick={onCloseMobile} aria-label={t('closeNavigation')} />}
    <aside className={navClass} aria-label={t('mainNavigation')}>
      <div className="nav-brand-row">
        <a href={appPath('/app/')} className="brand-link"><BrandMark compact={collapsed} animated /></a>
        <button className="icon-button nav-mobile-close" onClick={onCloseMobile} aria-label={t('closeNavigation')}><X /></button>
      </div>

      <nav className="nav-groups">
        <NavGroup title={t('workspace')} items={primary} collapsed={collapsed} />
        {isAdmin && <NavGroup title={t('manage')} items={manage} collapsed={collapsed} />}
        {isAdmin && <NavGroup title={t('advanced')} items={[
          { label: 'credentials', href: '/credentials', icon: KeyRound },
          { label: 'cache', href: '/cache', icon: Boxes },
        ]} collapsed={collapsed} />}
      </nav>

      <div className="nav-footer">
        <a className="nav-item" href={appPath('/feedback')} title={collapsed ? t('feedback') : undefined}>
          <Heart /><span>{t('feedback')}</span>
        </a>
        {isAdmin && <a className="nav-item" href={appPath('/system-updates')} title={collapsed ? t('settings') : undefined}>
          <Settings2 /><span>{t('settings')}</span>
        </a>}
        <button className="nav-item" onClick={onTheme} title={collapsed ? t('theme') : undefined}>
          {theme === 'dark' ? <Sun /> : <MoonStar />}<span>{theme === 'dark' ? t('lightMode') : t('darkMode')}</span>
        </button>
        <button className="nav-item" onClick={onLanguage} title={collapsed ? t('language') : undefined}>
          <Globe2 /><span>{getLanguage().toUpperCase()}</span>
        </button>
        <form method="post" action={appPath('/logout')}>
          <button className="nav-item sign-out" type="submit" title={collapsed ? t('signOut') : undefined}>
            <LogOut /><span>{t('signOut')}</span>
          </button>
        </form>
        <button className="nav-item nav-collapse" onClick={onCollapse} title={collapsed ? t('expandNavigation') : t('collapseNavigation')}>
          {collapsed ? <PanelLeftOpen /> : <PanelLeftClose />}<span>{t('collapseNavigation')}</span>
        </button>
      </div>
    </aside>
  </>
}

type NavGroupProps = {
  title: string
  items: Array<{ label: TranslationKey, href: string, icon: typeof MessageCircle, active?: boolean }>
  collapsed: boolean
}

function NavGroup({ title, items, collapsed }: NavGroupProps) {
  return <section className="nav-group">
    {!collapsed && <h2>{title}</h2>}
    {items.map(({ label, href, icon: Icon, active }) =>
      <a key={href} className={`nav-item${active ? ' is-active' : ''}`} href={appPath(href)} aria-current={active ? 'page' : undefined} title={collapsed ? t(label) : undefined}>
        <Icon /><span>{t(label)}</span>{active && <i aria-hidden="true" />}
      </a>)}
  </section>
}

export function MobileBackButton({ onClick }: { onClick: () => void }) {
  return <button className="icon-button mobile-back" onClick={onClick} aria-label={t('backToConversations')}><ChevronLeft /></button>
}
