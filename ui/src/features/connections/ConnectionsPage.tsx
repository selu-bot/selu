import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  AlertTriangle, Bot, CheckCircle2, Cloud, Cpu, List, Pencil, PlugZap, ShieldCheck, Trash2,
} from 'lucide-react'
import { ApiError } from '../../api'
import { defineTranslations, useTranslations } from '../../i18n'
import { useNotices, useQueryErrorNotice } from '../../notices'
import { Button, ConfirmDialog, EmptyState, Field, Input, PageHeader, SecretField, StatusBadge } from '../../shared/ui'
import { AppPageShell } from '../shell/AppPageShell'
import { ManagementLoading, ManagementSheet, OverviewCard, OverviewGrid } from '../management/Management'
import { connectionsApi, type Provider, type ProviderConfiguration } from './api'
import './ConnectionsPage.css'

const messages = defineTranslations({
  eyebrow: 'AI services', title: 'Connections', description: 'Connect the AI services Selu can use. Access keys stay encrypted and are never shown again.', connected: 'Connected', notConnected: 'Not connected', cloud: 'Online service', local: 'On your network', configure: 'Connect', edit: 'Edit', test: 'Test', testing: 'Testing…', tested: 'Connection works', models: 'Models', remove: 'Disconnect', removeTitle: 'Disconnect this service?', removeBody: 'Selu will stop using this service. The saved access key and address will be removed.', disconnected: 'Service disconnected', configurationTitle: 'Connect {name}', configurationDescription: 'Enter the private details supplied by this service.', apiKey: 'Access key', apiKeyExisting: 'Leave blank to keep the currently saved key.', apiKeyNew: 'Paste the access key from your provider.', region: 'AWS region', address: 'Server address', advanced: 'Connection details', save: 'Save connection', cancel: 'Cancel', close: 'Close panel', saved: 'Connection saved', saveError: 'The connection was not saved', loadError: 'Connections could not be loaded', retry: 'Try again', forbidden: 'Only an administrator can manage AI services.', forbiddenBody: 'Ask an administrator if an AI service needs to be connected or changed.', noProviders: 'No AI services available', noProvidersBody: 'This Selu does not currently have any supported services to connect.', modelsTitle: 'Models from {name}', modelsDescription: 'Models this service currently makes available to Selu.', noModels: 'No models were returned by this service.', modelsError: 'Models could not be loaded', modelsErrorBody: 'Check that this service is connected, then try again.', token: 'Bedrock access token', tokenHint: 'Amazon Bedrock API keys use bearer-token authentication.', required: 'Complete the required connection details.', secretPrivate: 'This value is encrypted and will not be displayed again.', show: 'Show value', hide: 'Hide value',
}, {
  eyebrow: 'KI-Dienste', title: 'Verbindungen', description: 'Verbinde die KI-Dienste, die Selu verwenden kann. Zugangsschlüssel bleiben verschlüsselt und werden nie wieder angezeigt.', connected: 'Verbunden', notConnected: 'Nicht verbunden', cloud: 'Online-Dienst', local: 'In deinem Netzwerk', configure: 'Verbinden', edit: 'Bearbeiten', test: 'Testen', testing: 'Wird getestet…', tested: 'Verbindung funktioniert', models: 'Modelle', remove: 'Trennen', removeTitle: 'Diesen Dienst trennen?', removeBody: 'Selu verwendet diesen Dienst danach nicht mehr. Der gespeicherte Zugangsschlüssel und die Adresse werden entfernt.', disconnected: 'Dienst getrennt', configurationTitle: '{name} verbinden', configurationDescription: 'Gib die privaten Angaben ein, die du von diesem Dienst erhalten hast.', apiKey: 'Zugangsschlüssel', apiKeyExisting: 'Leer lassen, um den gespeicherten Schlüssel zu behalten.', apiKeyNew: 'Füge den Zugangsschlüssel deines Anbieters ein.', region: 'AWS-Region', address: 'Serveradresse', advanced: 'Verbindungsdetails', save: 'Verbindung speichern', cancel: 'Abbrechen', close: 'Bereich schließen', saved: 'Verbindung gespeichert', saveError: 'Die Verbindung wurde nicht gespeichert', loadError: 'Verbindungen konnten nicht geladen werden', retry: 'Erneut versuchen', forbidden: 'Nur Administratoren können KI-Dienste verwalten.', forbiddenBody: 'Bitte einen Administrator, wenn ein KI-Dienst verbunden oder geändert werden muss.', noProviders: 'Keine KI-Dienste verfügbar', noProvidersBody: 'Für dieses Selu stehen zurzeit keine unterstützten Dienste bereit.', modelsTitle: 'Modelle von {name}', modelsDescription: 'Modelle, die dieser Dienst Selu gerade zur Verfügung stellt.', noModels: 'Dieser Dienst hat keine Modelle zurückgegeben.', modelsError: 'Modelle konnten nicht geladen werden', modelsErrorBody: 'Prüfe, ob dieser Dienst verbunden ist, und versuche es erneut.', token: 'Bedrock-Zugangstoken', tokenHint: 'Amazon-Bedrock-API-Schlüssel verwenden eine Bearer-Token-Anmeldung.', required: 'Fülle die erforderlichen Verbindungsangaben aus.', secretPrivate: 'Dieser Wert wird verschlüsselt und danach nicht mehr angezeigt.', show: 'Wert anzeigen', hide: 'Wert ausblenden',
})
type Copy = { [K in keyof typeof messages.en]: string }

export function ConnectionsPage() {
  const copy = useTranslations(messages)
  const notices = useNotices()
  const cache = useQueryClient()
  const providers = useQuery({ queryKey: ['providers'], queryFn: connectionsApi.list, retry: false })
  const forbidden = providers.error instanceof ApiError && providers.error.status === 403
  useQueryErrorNotice(forbidden ? null : providers.error, copy.loadError)
  const [editing, setEditing] = useState<Provider | null>(null)
  const [deleting, setDeleting] = useState<Provider | null>(null)
  const [modelsFor, setModelsFor] = useState<Provider | null>(null)
  const refresh = () => cache.invalidateQueries({ queryKey: ['providers'] })
  const configure = useMutation({
    mutationFn: ({ provider, input }: { provider: Provider; input: ProviderConfiguration }) => connectionsApi.configure(provider.id, input),
    onSuccess: () => { setEditing(null); void refresh(); notices.success(copy.saved) },
    onError: (error) => notices.error(error, copy.saveError),
  })
  const remove = useMutation({
    mutationFn: (id: string) => connectionsApi.remove(id),
    onSuccess: () => { setDeleting(null); void refresh(); notices.success(copy.disconnected) },
    onError: (error) => notices.error(error, copy.saveError),
  })
  const test = useMutation({
    mutationFn: (id: string) => connectionsApi.test(id),
    onSuccess: () => notices.success(copy.tested),
    onError: (error) => notices.error(error, copy.saveError),
  })

  return <AppPageShell active="connections" width="wide">
    <PageHeader eyebrow={copy.eyebrow} title={copy.title} description={copy.description} />
    {providers.isPending ? <ManagementLoading /> : forbidden ?
      <EmptyState icon={<ShieldCheck />} title={copy.forbidden} description={copy.forbiddenBody} /> :
      providers.isError ?
        <EmptyState icon={<AlertTriangle />} title={copy.loadError} action={<Button onClick={() => void providers.refetch()}>{copy.retry}</Button>} /> :
        providers.data?.length ? <OverviewGrid>{providers.data.map((provider) => <OverviewCard
          key={provider.id}
          icon={provider.kind === 'local' ? <Cpu /> : <Cloud />}
          status={<StatusBadge tone={provider.configured ? 'success' : 'neutral'}>{provider.configured ? copy.connected : copy.notConnected}</StatusBadge>}
          title={provider.display_name}
          description={provider.kind === 'local' ? copy.local : copy.cloud}
          meta={provider.base_url || provider.default_base_url || '—'}
          actions={<>
            <Button size="sm" variant={provider.configured ? 'secondary' : 'primary'} leadingIcon={provider.configured ? <Pencil /> : <PlugZap />} onClick={() => setEditing(provider)}>{provider.configured ? copy.edit : copy.configure}</Button>
            {provider.configured && <Button size="sm" variant="ghost" leadingIcon={<CheckCircle2 />} loading={test.isPending && test.variables === provider.id} onClick={() => test.mutate(provider.id)}>{copy.test}</Button>}
            {provider.configured && <Button size="sm" variant="ghost" leadingIcon={<List />} onClick={() => setModelsFor(provider)}>{copy.models}</Button>}
            {provider.configured && <Button size="sm" variant="ghost" leadingIcon={<Trash2 />} onClick={() => setDeleting(provider)}>{copy.remove}</Button>}
          </>}
        />)}</OverviewGrid> : <EmptyState icon={<Bot />} title={copy.noProviders} description={copy.noProvidersBody} />}
    <ProviderSheet provider={editing} copy={copy} busy={configure.isPending} onClose={() => setEditing(null)} onSave={(input) => editing && configure.mutate({ provider: editing, input })} />
    <ModelsSheet provider={modelsFor} copy={copy} onClose={() => setModelsFor(null)} />
    <ConfirmDialog open={Boolean(deleting)} title={copy.removeTitle} message={copy.removeBody} confirmLabel={copy.remove} cancelLabel={copy.cancel} destructive busy={remove.isPending} onCancel={() => setDeleting(null)} onConfirm={() => deleting && remove.mutate(deleting.id)} />
  </AppPageShell>
}

function ProviderSheet({ provider, copy, busy, onClose, onSave }: { provider: Provider | null; copy: Copy; busy: boolean; onClose: () => void; onSave: (input: ProviderConfiguration) => void }) {
  const [key, setKey] = useState('')
  const [baseUrl, setBaseUrl] = useState('')
  const [error, setError] = useState('')
  useEffect(() => {
    if (provider) {
      setKey('')
      setBaseUrl(provider.base_url ?? provider.default_base_url ?? '')
      setError('')
    }
  }, [provider])
  const save = () => {
    if (!provider) return
    if ((provider.requires_api_key && !provider.has_api_key && !key.trim()) || (provider.requires_base_url && !baseUrl.trim())) {
      setError(copy.required)
      return
    }
    onSave({ api_key: key.trim() || undefined, base_url: provider.requires_base_url ? baseUrl.trim() : undefined })
  }
  return <ManagementSheet
    open={Boolean(provider)}
    title={copy.configurationTitle.replace('{name}', provider?.display_name ?? '')}
    description={copy.configurationDescription}
    closeLabel={copy.close}
    onClose={onClose}
    busy={busy}
    actions={<><Button onClick={onClose} disabled={busy}>{copy.cancel}</Button><Button variant="primary" loading={busy} onClick={save}>{copy.save}</Button></>}
  >
    <div className="management-form">
      {error && <div className="management-inline-error" role="alert">{error}</div>}
      {provider?.requires_api_key && <Field label={provider.id === 'bedrock' ? copy.token : copy.apiKey} hint={<>{provider.has_api_key ? copy.apiKeyExisting : copy.apiKeyNew}<br />{provider.id === 'bedrock' ? copy.tokenHint : copy.secretPrivate}</>}><SecretField value={key} onChange={(event) => { setKey(event.target.value); setError('') }} autoComplete="off" showLabel={copy.show} hideLabel={copy.hide} /></Field>}
      {provider?.requires_base_url && <details open><summary>{copy.advanced}</summary><div><Field label={provider.id === 'bedrock' ? copy.region : copy.address}><Input value={baseUrl} onChange={(event) => { setBaseUrl(event.target.value); setError('') }} placeholder={provider.default_base_url ?? undefined} /></Field></div></details>}
    </div>
  </ManagementSheet>
}

function ModelsSheet({ provider, copy, onClose }: { provider: Provider | null; copy: Copy; onClose: () => void }) {
  const models = useQuery({
    queryKey: ['provider-models', provider?.id],
    queryFn: () => connectionsApi.models(provider!.id),
    enabled: Boolean(provider),
    retry: false,
  })
  return <ManagementSheet
    open={Boolean(provider)}
    title={copy.modelsTitle.replace('{name}', provider?.display_name ?? '')}
    description={copy.modelsDescription}
    closeLabel={copy.close}
    onClose={onClose}
  >
    <div className="provider-models">
      {models.isPending ? <ManagementLoading cards={2} /> : models.isError ?
        <EmptyState icon={<AlertTriangle />} title={copy.modelsError} description={copy.modelsErrorBody} action={<Button loading={models.isFetching} onClick={() => void models.refetch()}>{copy.retry}</Button>} /> :
        models.data?.length ? models.data.map((model) => <div key={model.id}><Bot /><span><strong>{model.name}</strong><small>{model.id}</small></span></div>) :
          <p className="management-muted">{copy.noModels}</p>}
    </div>
  </ManagementSheet>
}
