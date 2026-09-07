import {
  Bot, CalendarClock, Database, Globe2, Heart, Home, LogOut,
  MessageCircle, MoonStar, PanelLeftClose, PanelLeftOpen, RefreshCw, Settings2, ShieldCheck,
  Sun, Users, Waypoints, X,
} from 'lucide-react'
import { Link } from '@tanstack/react-router'
import { getLanguage, t, type TranslationKey } from '../i18n'
import { BrandMark } from './BrandMark'

export type AppNavigationProps = {
  active: 'home' | 'conversations' | 'automations' | 'agents' | 'connectors' | 'updates' | 'connections' | 'about-you' | 'people' | 'feedback' | 'settings'
  collapsed: boolean
  mobileOpen: boolean
  theme: 'light' | 'dark'
  isAdmin: boolean
  onCollapse: () => void
  onCloseMobile: () => void
  onLanguage: () => void
  onTheme: () => void
  onLogout: () => void
}

export function AppNavigation(props: AppNavigationProps) {
  const { active, collapsed, isAdmin, mobileOpen, theme, onCollapse, onCloseMobile, onLanguage, onTheme, onLogout } = props
  const navClass = `app-navigation${collapsed ? ' is-collapsed' : ''}${mobileOpen ? ' is-mobile-open' : ''}`

  return <>
    {mobileOpen && <button className="nav-scrim" onClick={onCloseMobile} aria-label={t('closeNavigation')} />}
    <aside className={navClass} aria-label={t('mainNavigation')}>
      <div className="nav-brand-row">
        <Link to="/app" className="brand-link" aria-label={t('home')}><BrandMark compact={collapsed} animated /></Link>
        <button className="icon-button nav-mobile-close" onClick={onCloseMobile} aria-label={t('closeNavigation')}><X /></button>
      </div>

      <nav className="nav-groups">
        <section className="nav-group">
          {!collapsed && <h2>{t('workspace')}</h2>}
          <RouteItem label="home" to="/app" icon={Home} active={active === 'home'} collapsed={collapsed} />
          <RouteItem label="conversations" to="/app/conversations" icon={MessageCircle} active={active === 'conversations'} collapsed={collapsed} />
          <RouteItem label="schedules" to="/app/automations" icon={CalendarClock} active={active === 'automations'} collapsed={collapsed} />
          <RouteItem label="memory" to="/app/about-you" icon={Database} active={active === 'about-you'} collapsed={collapsed} />
        </section>
        {isAdmin && <section className="nav-group">
          {!collapsed && <h2>{t('manage')}</h2>}
          <RouteItem label="agents" to="/app/agents" icon={Bot} active={active === 'agents'} collapsed={collapsed} />
          <RouteItem label="connectors" to="/app/connectors" icon={Waypoints} active={active === 'connectors'} collapsed={collapsed} />
          <RouteItem label="providers" to="/app/connections" icon={ShieldCheck} active={active === 'connections'} collapsed={collapsed} />
          <RouteItem label="people" to="/app/people" icon={Users} active={active === 'people'} collapsed={collapsed} />
          <RouteItem label="updates" to="/app/updates" icon={RefreshCw} active={active === 'updates'} collapsed={collapsed} />
        </section>}
      </nav>

      <div className="nav-footer">
        <RouteItem label="feedback" to="/app/feedback" icon={Heart} active={active === 'feedback'} collapsed={collapsed} />
        <RouteItem label="settings" to="/app/settings" icon={Settings2} active={active === 'settings'} collapsed={collapsed} />
        <button className="nav-item" onClick={onTheme} title={collapsed ? t('theme') : undefined}>
          {theme === 'dark' ? <Sun /> : <MoonStar />}<span>{theme === 'dark' ? t('lightMode') : t('darkMode')}</span>
        </button>
        <button className="nav-item" onClick={onLanguage} title={collapsed ? t('language') : undefined}>
          <Globe2 /><span>{getLanguage().toUpperCase()}</span>
        </button>
        <button className="nav-item sign-out" type="button" onClick={onLogout} title={collapsed ? t('signOut') : undefined}>
          <LogOut /><span>{t('signOut')}</span>
        </button>
        <button className="nav-item nav-collapse" onClick={onCollapse} title={collapsed ? t('expandNavigation') : t('collapseNavigation')}>
          {collapsed ? <PanelLeftOpen /> : <PanelLeftClose />}<span>{t('collapseNavigation')}</span>
        </button>
      </div>
    </aside>
  </>
}

type SpaPath = '/app' | '/app/conversations' | '/app/automations' | '/app/agents' | '/app/connectors' | '/app/updates' | '/app/connections' | '/app/about-you' | '/app/people' | '/app/settings' | '/app/feedback'

function RouteItem({ label, to, icon: Icon, active, collapsed }: { label: TranslationKey; to: SpaPath; icon: typeof MessageCircle; active: boolean; collapsed: boolean }) {
  return <Link className={`nav-item${active ? ' is-active' : ''}`} to={to} aria-current={active ? 'page' : undefined} title={collapsed ? t(label) : undefined}>
    <Icon /><span>{t(label)}</span>{active && <i aria-hidden="true" />}
  </Link>
}

export function MobileBackButton({ onClick }: { onClick: () => void }) {
  return <button className="icon-button mobile-back" onClick={onClick} aria-label={t('backToConversations')}><span aria-hidden="true">‹</span></button>
}
