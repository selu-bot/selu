import type { ReactNode } from 'react'
import { Menu } from 'lucide-react'
import { BrandMark } from '../../components/BrandMark'
import { t } from '../../i18n'
import { PageLayout, type PageLayoutProps } from '../../shared/ui'
import { useAppChrome, type AppSection } from './useAppChrome'
import './AppPageShell.css'

export type AppPageShellProps = {
  active: AppSection
  children: ReactNode
  width?: PageLayoutProps['width']
  sidebar?: ReactNode
  aside?: ReactNode
}

export function AppPageShell({ active, children, width = 'default', sidebar, aside }: AppPageShellProps) {
  const { navigation, navCollapsed, openMobileNavigation } = useAppChrome(active)

  return <div className={`app-page-shell${navCollapsed ? ' nav-collapsed' : ''}`}>
    {navigation}
    <section className="app-page-surface">
      <header className="app-page-topbar">
        <button className="icon-button mobile-menu" onClick={openMobileNavigation} aria-label={t('openNavigation')}><Menu /></button>
        <BrandMark compact />
      </header>
      <div className="app-page-scroll">
        <PageLayout width={width} sidebar={sidebar} aside={aside}>{children}</PageLayout>
      </div>
    </section>
  </div>
}
