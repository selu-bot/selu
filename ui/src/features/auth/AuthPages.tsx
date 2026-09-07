import { useState, type FormEvent } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { useNavigate } from '@tanstack/react-router'
import { ArrowRight, Eye, EyeOff, LockKeyhole, UserRound } from 'lucide-react'
import { api, type LoginInput, type SetupInput } from '../../api'
import { getLanguage, t } from '../../i18n'
import { describeError } from '../../notices'
import { BrandMark } from '../../components/BrandMark'

export const authStateQuery = {
  queryKey: ['auth', 'state'] as const,
  queryFn: api.authState,
  staleTime: 30_000,
}

export function LoginPage() {
  return <AuthPage mode="login" />
}

export function SetupPage() {
  return <AuthPage mode="setup" />
}

function AuthPage({ mode }: { mode: 'login' | 'setup' }) {
  const cache = useQueryClient()
  const navigate = useNavigate()
  const [displayName, setDisplayName] = useState('')
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [showPassword, setShowPassword] = useState(false)
  const mutation = useMutation({
    mutationFn: () => mode === 'login'
      ? api.login({ username: username.trim(), password } satisfies LoginInput)
      : api.setup({ display_name: displayName.trim(), username: username.trim(), password, language: getLanguage() } satisfies SetupInput),
    onSuccess: async (state) => {
      cache.setQueryData(authStateQuery.queryKey, state)
      await cache.invalidateQueries({ queryKey: ['session'] })
      await navigate({ to: '/app' })
    },
  })
  const submit = (event: FormEvent) => {
    event.preventDefault()
    if (!mutation.isPending && username.trim() && password && (mode === 'login' || displayName.trim())) mutation.mutate()
  }
  const invalid = !username.trim() || !password || (mode === 'setup' && !displayName.trim())
  const error = mutation.error ? describeError(mutation.error) : null

  return <main className="auth-shell">
    <section className="auth-card" aria-labelledby="auth-title">
      <BrandMark animated />
      <div className="auth-heading">
        <span className="eyebrow">{mode === 'login' ? t('welcomeBack') : t('firstRun')}</span>
        <h1 id="auth-title">{mode === 'login' ? t('loginTitle') : t('setupTitle')}</h1>
        <p>{mode === 'login' ? t('loginBody') : t('setupBody')}</p>
      </div>
      <form onSubmit={submit}>
        {mode === 'setup' && <label>
          <span>{t('displayName')}</span>
          <div className="auth-field"><UserRound aria-hidden="true" /><input autoComplete="name" value={displayName} onChange={(event) => setDisplayName(event.target.value)} /></div>
        </label>}
        <label>
          <span>{t('username')}</span>
          <div className="auth-field"><UserRound aria-hidden="true" /><input autoFocus={mode === 'login'} autoCapitalize="none" autoComplete="username" value={username} onChange={(event) => setUsername(event.target.value)} /></div>
        </label>
        <label>
          <span>{t('password')}</span>
          <div className="auth-field"><LockKeyhole aria-hidden="true" /><input type={showPassword ? 'text' : 'password'} autoComplete={mode === 'login' ? 'current-password' : 'new-password'} value={password} onChange={(event) => setPassword(event.target.value)} />
            <button type="button" className="auth-reveal" onClick={() => setShowPassword(!showPassword)} aria-label={showPassword ? t('hidePassword') : t('showPassword')}>{showPassword ? <EyeOff /> : <Eye />}</button>
          </div>
        </label>
        {error && <div className="auth-error" role="alert"><strong>{error.title}</strong><span>{error.body}</span></div>}
        <button className="auth-submit" disabled={invalid || mutation.isPending}>{mutation.isPending ? t('signingIn') : mode === 'login' ? t('signIn') : t('finishSetup')}<ArrowRight /></button>
      </form>
      <p className="auth-footnote"><LockKeyhole />{t('authPrivate')}</p>
    </section>
  </main>
}
