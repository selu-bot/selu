import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { AlertTriangle, CheckCircle2, Download, RefreshCw, RotateCcw, Settings2, ShieldCheck, UploadCloud } from 'lucide-react'
import { ApiError } from '../../api'
import { defineTranslations, useLanguage, useTranslations } from '../../i18n'
import { useNotices, useQueryErrorNotice } from '../../notices'
import { Button, ConfirmDialog, EmptyState, Field, PageHeader, Select, StatusBadge } from '../../shared/ui'
import { ManagementLoading, ManagementSection, ManagementSheet, OverviewCard, OverviewGrid } from '../management/Management'
import '../management/final-management.css'
import { AppPageShell } from '../shell/AppPageShell'
import { updatesApi, type UpdateSettings, type UpdateSettingsInput, type UpdateStatus } from './api'
import { DockerStorageSection } from './DockerStorageSection'

const messages = defineTranslations({
  eyebrow: 'Your Selu', title: 'System updates', description: 'Keep Selu current, see exactly what is happening, and return to the previous version if needed.', current: 'Installed version', available: 'Available version', previous: 'Previous version', upToDate: 'Up to date', updateReady: 'Update ready', needsAttention: 'Needs attention', checking: 'Checking…', checkNow: 'Check now', update: 'Install update', rollback: 'Return to previous version', settings: 'Update settings', channel: 'Release channel', channelHint: 'Stable is recommended. Unstable receives changes earlier.', automatic: 'Install updates automatically', automaticHint: 'Selu installs new releases after they pass the built-in check.', push: 'Mobile update alerts', pushHint: 'Let paired phones know when an update needs attention.', telemetry: 'Anonymous installation statistics', telemetryHint: 'Share basic installation health without conversations or private details.', save: 'Save settings', saved: 'Update settings saved', cancel: 'Cancel', close: 'Close panel', loadError: 'System updates could not be loaded', forbidden: 'Only an administrator can manage updates.', forbiddenBody: 'Ask an administrator to check or install Selu updates.', retry: 'Try again', checked: 'Update check complete', checkFailed: 'Selu could not check for updates', updateTitle: 'Install this update?', updateBody: 'Selu will download the new version and restart. Conversations stay saved, but the page may be unavailable for a short time.', rollbackTitle: 'Return to the previous version?', rollbackBody: 'Selu will restart using the last saved version. Your conversations and settings stay in place.', updating: 'Updating Selu', rollingBack: 'Returning to the previous version', reconnecting: 'Selu is restarting. This page will reconnect automatically.', progressIdle: 'Ready for the next update check.', progressChecking: 'Looking for a newer release.', progressPreparing: 'Preparing the update.', progressPulling: 'Downloading the new version.', progressRestarting: 'Restarting Selu.', progressHealth: 'Making sure everything works.', progressDone: 'Update installed successfully.', progressFailed: 'The update did not finish.', serviceUnavailable: 'Selu could not reach the update service. Check that it is running, then try again.', progressRollback: 'Restoring the previous version.', lastChecked: 'Last checked {date}', neverChecked: 'Not checked yet', changelog: 'What changed', openNotes: 'Open full release notes', noNotes: 'No release notes were provided for this version.', lastError: 'What happened', saveFailed: 'Update settings were not saved', applyFailed: 'The update could not be started', rollbackFailed: 'The previous version could not be restored', started: 'Update started', rollbackStarted: 'Rollback started', privacyOn: 'Sharing is on', privacyOff: 'Sharing is off', enabled: 'On', disabled: 'Off', status: 'Update status', unknownVersion: 'Version unavailable', releaseDetails: 'Release details', safeRollback: 'A previous version is ready if you need it.', noRollback: 'No previous version is available yet.',
}, {
  eyebrow: 'Dein Selu', title: 'Systemaktualisierungen', description: 'Halte Selu aktuell, verfolge jeden Schritt und kehre bei Bedarf zur vorherigen Version zurück.', current: 'Installierte Version', available: 'Verfügbare Version', previous: 'Vorherige Version', upToDate: 'Aktuell', updateReady: 'Aktualisierung bereit', needsAttention: 'Prüfung nötig', checking: 'Wird geprüft…', checkNow: 'Jetzt prüfen', update: 'Aktualisierung installieren', rollback: 'Zur vorherigen Version', settings: 'Aktualisierungseinstellungen', channel: 'Veröffentlichungskanal', channelHint: 'Stabil wird empfohlen. Instabil erhält Änderungen früher.', automatic: 'Aktualisierungen automatisch installieren', automaticHint: 'Selu installiert neue Versionen nach der eingebauten Prüfung.', push: 'Mobile Aktualisierungshinweise', pushHint: 'Informiere verbundene Handys, wenn eine Aktualisierung Aufmerksamkeit braucht.', telemetry: 'Anonyme Installationsstatistiken', telemetryHint: 'Teile grundlegende Installationsdaten – ohne Unterhaltungen oder private Angaben.', save: 'Einstellungen speichern', saved: 'Aktualisierungseinstellungen gespeichert', cancel: 'Abbrechen', close: 'Bereich schließen', loadError: 'Systemaktualisierungen konnten nicht geladen werden', forbidden: 'Nur Administratoren können Aktualisierungen verwalten.', forbiddenBody: 'Bitte einen Administrator, Selu zu prüfen oder zu aktualisieren.', retry: 'Erneut versuchen', checked: 'Aktualisierungsprüfung abgeschlossen', checkFailed: 'Selu konnte nicht nach Aktualisierungen suchen', updateTitle: 'Diese Aktualisierung installieren?', updateBody: 'Selu lädt die neue Version herunter und startet neu. Unterhaltungen bleiben gespeichert, die Seite kann aber kurz nicht erreichbar sein.', rollbackTitle: 'Zur vorherigen Version zurückkehren?', rollbackBody: 'Selu startet mit der zuletzt gespeicherten Version neu. Unterhaltungen und Einstellungen bleiben erhalten.', updating: 'Selu wird aktualisiert', rollingBack: 'Vorherige Version wird wiederhergestellt', reconnecting: 'Selu startet neu. Diese Seite verbindet sich automatisch wieder.', progressIdle: 'Bereit für die nächste Aktualisierungsprüfung.', progressChecking: 'Eine neuere Version wird gesucht.', progressPreparing: 'Die Aktualisierung wird vorbereitet.', progressPulling: 'Die neue Version wird heruntergeladen.', progressRestarting: 'Selu wird neu gestartet.', progressHealth: 'Es wird geprüft, ob alles funktioniert.', progressDone: 'Aktualisierung erfolgreich installiert.', progressFailed: 'Die Aktualisierung wurde nicht abgeschlossen.', serviceUnavailable: 'Selu konnte den Aktualisierungsdienst nicht erreichen. Prüfe, ob er läuft, und versuche es erneut.', progressRollback: 'Die vorherige Version wird wiederhergestellt.', lastChecked: 'Zuletzt geprüft: {date}', neverChecked: 'Noch nicht geprüft', changelog: 'Was ist neu?', openNotes: 'Vollständige Versionshinweise öffnen', noNotes: 'Für diese Version wurden keine Hinweise bereitgestellt.', lastError: 'Was passiert ist', saveFailed: 'Aktualisierungseinstellungen wurden nicht gespeichert', applyFailed: 'Die Aktualisierung konnte nicht gestartet werden', rollbackFailed: 'Die vorherige Version konnte nicht wiederhergestellt werden', started: 'Aktualisierung gestartet', rollbackStarted: 'Wiederherstellung gestartet', privacyOn: 'Freigabe ist an', privacyOff: 'Freigabe ist aus', enabled: 'An', disabled: 'Aus', status: 'Aktualisierungsstatus', unknownVersion: 'Version nicht verfügbar', releaseDetails: 'Versionsdetails', safeRollback: 'Eine vorherige Version steht bei Bedarf bereit.', noRollback: 'Es ist noch keine vorherige Version verfügbar.',
})
type Copy = { [K in keyof typeof messages.en]: string }

export function UpdatesPage() {
  const copy = useTranslations(messages), language = useLanguage(), notices = useNotices(), cache = useQueryClient()
  const settings = useQuery({ queryKey: ['system-updates', 'settings'], queryFn: updatesApi.settings, retry: false })
  const status = useQuery({ queryKey: ['system-updates', 'status'], queryFn: updatesApi.status, retry: true, refetchInterval: (query) => query.state.data?.active_job_id ? 1_500 : 15_000 })
  const forbidden = settings.error instanceof ApiError && settings.error.status === 403
  useQueryErrorNotice(forbidden ? null : settings.error ?? (status.data?.active_job_id ? null : status.error), copy.loadError)
  const [editing, setEditing] = useState(false), [confirming, setConfirming] = useState<'apply' | 'rollback' | null>(null)
  const refresh = () => Promise.all([cache.invalidateQueries({ queryKey: ['system-updates', 'settings'] }), cache.invalidateQueries({ queryKey: ['system-updates', 'status'] })])
  const markStarted = (action: 'apply' | 'rollback') => cache.setQueryData<UpdateStatus>(['system-updates', 'status'], (current) => current ? optimisticUpdateStatus(current, action) : current)
  const check = useMutation({ mutationFn: updatesApi.check, onSuccess: () => { notices.success(copy.checked); void refresh() }, onError: (error) => notices.error(error, copy.checkFailed) })
  const apply = useMutation({ mutationFn: updatesApi.apply, onSuccess: () => { markStarted('apply'); setConfirming(null); notices.info(copy.started); void refresh() }, onError: (error) => notices.error(error, copy.applyFailed) })
  const rollback = useMutation({ mutationFn: updatesApi.rollback, onSuccess: () => { markStarted('rollback'); setConfirming(null); notices.info(copy.rollbackStarted); void refresh() }, onError: (error) => notices.error(error, copy.rollbackFailed) })

  return <AppPageShell active="updates" width="wide">
    <PageHeader eyebrow={copy.eyebrow} title={copy.title} description={copy.description} actions={<Button leadingIcon={<Settings2 />} onClick={() => setEditing(true)}>{copy.settings}</Button>} />
    {settings.isPending || status.isPending ? <ManagementLoading /> : forbidden ? <EmptyState icon={<ShieldCheck />} title={copy.forbidden} description={copy.forbiddenBody} /> : !settings.data || !status.data ? <EmptyState icon={<AlertTriangle />} title={copy.loadError} action={<Button onClick={() => void refresh()}>{copy.retry}</Button>} /> : <UpdatesContent copy={copy} language={language} settings={settings.data} status={status.data} checking={check.isPending} onCheck={() => check.mutate()} onApply={() => setConfirming('apply')} onRollback={() => setConfirming('rollback')} />}
    {settings.data && <SettingsSheet open={editing} copy={copy} settings={settings.data} onClose={() => setEditing(false)} onSaved={() => { setEditing(false); notices.success(copy.saved); void refresh() }} />}
    <ConfirmDialog open={confirming === 'apply'} title={copy.updateTitle} message={copy.updateBody} confirmLabel={copy.update} cancelLabel={copy.cancel} busy={apply.isPending} onCancel={() => setConfirming(null)} onConfirm={() => apply.mutate()} />
    <ConfirmDialog open={confirming === 'rollback'} title={copy.rollbackTitle} message={copy.rollbackBody} confirmLabel={copy.rollback} cancelLabel={copy.cancel} destructive busy={rollback.isPending} onCancel={() => setConfirming(null)} onConfirm={() => rollback.mutate()} />
  </AppPageShell>
}

function UpdatesContent({ copy, language, settings, status, checking, onCheck, onApply, onRollback }: { copy: Copy; language: string; settings: UpdateSettings; status: UpdateStatus; checking: boolean; onCheck: () => void; onApply: () => void; onRollback: () => void }) {
  const running = Boolean(status.active_job_id), failed = status.status === 'failed', percent = progressPercent(status.progress_key, running)
  return <div className="final-stack">
    <OverviewGrid>
      <OverviewCard icon={<CheckCircle2 />} status={<StatusBadge tone={failed ? 'danger' : 'success'}>{failed ? copy.needsAttention : copy.current}</StatusBadge>} title={status.installed_display || copy.unknownVersion} description={copy.current} meta={status.last_checked_at ? copy.lastChecked.replace('{date}', formatDate(status.last_checked_at, language)) : copy.neverChecked} />
      <OverviewCard icon={<Download />} status={<StatusBadge tone={status.update_available ? 'warning' : 'success'}>{status.update_available ? copy.updateReady : copy.upToDate}</StatusBadge>} title={status.available_display || status.installed_display || copy.unknownVersion} description={copy.available} actions={<><Button size="sm" leadingIcon={<RefreshCw />} loading={checking} onClick={onCheck}>{copy.checkNow}</Button>{status.update_available && <Button size="sm" variant="primary" leadingIcon={<UploadCloud />} onClick={onApply}>{copy.update}</Button>}</>} />
      <OverviewCard icon={<RotateCcw />} status={<StatusBadge tone={status.rollback_available ? 'info' : 'neutral'}>{status.rollback_available ? copy.previous : copy.upToDate}</StatusBadge>} title={status.previous_display || '—'} description={status.rollback_available ? copy.safeRollback : copy.noRollback} actions={status.rollback_available ? <Button size="sm" variant="ghost" onClick={onRollback}>{copy.rollback}</Button> : undefined} />
      <OverviewCard icon={<Settings2 />} status={<StatusBadge tone="info">{settings.auto_update ? copy.enabled : copy.disabled}</StatusBadge>} title={settings.release_channel} description={copy.channel} meta={`${copy.automatic}: ${settings.auto_update ? copy.enabled : copy.disabled}`} />
    </OverviewGrid>
    {(running || failed) && <ManagementSection title={running ? (status.progress_key.includes('rollback') ? copy.rollingBack : copy.updating) : copy.needsAttention} description={running ? copy.reconnecting : copy.serviceUnavailable}>
      <div className="final-section-stack"><div className="final-progress" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent}><span style={{ width: `${percent}%` }} /></div><p className={failed ? 'final-callout is-danger' : 'final-callout'}>{progressText(copy, status.progress_key)}</p></div>
    </ManagementSection>}
    <DockerStorageSection systemUpdateActive={running} />
    <ManagementSection title={copy.releaseDetails} description={status.available_display || status.installed_display} actions={status.available_changelog_url ? <a className="final-detail-link" href={status.available_changelog_url} target="_blank" rel="noreferrer">{copy.openNotes}</a> : undefined}>
      <div className="final-changelog">{status.available_changelog_body || copy.noNotes}</div>
    </ManagementSection>
    {status.last_error && !failed && <ManagementSection title={copy.lastError}><div className="final-callout is-danger">{copy.serviceUnavailable}</div></ManagementSection>}
  </div>
}

function SettingsSheet({ open, copy, settings, onClose, onSaved }: { open: boolean; copy: Copy; settings: UpdateSettings; onClose: () => void; onSaved: () => void }) {
  const notices = useNotices()
  const [channel, setChannel] = useState(settings.release_channel)
  const [automatic, setAutomatic] = useState(settings.auto_update)
  const [push, setPush] = useState(settings.push_notifications_enabled)
  const [telemetry, setTelemetry] = useState(!settings.installation_telemetry_opt_out)
  useEffect(() => {
    if (open) {
      setChannel(settings.release_channel)
      setAutomatic(settings.auto_update)
      setPush(settings.push_notifications_enabled)
      setTelemetry(!settings.installation_telemetry_opt_out)
    }
  }, [open, settings])
  const save = useMutation({
    mutationFn: (input: UpdateSettingsInput) => updatesApi.save(input),
    onSuccess: onSaved,
    onError: (error) => notices.error(error, copy.saveFailed),
  })
  const submit = () => save.mutate({ release_channel: channel, auto_update: automatic, push_notifications_enabled: push, installation_telemetry_opt_out: !telemetry })
  return <ManagementSheet open={open} title={copy.settings} description={copy.description} closeLabel={copy.close} onClose={onClose} busy={save.isPending} actions={<><Button onClick={onClose}>{copy.cancel}</Button><Button variant="primary" loading={save.isPending} onClick={submit}>{copy.save}</Button></>}>
    <div className="management-form">
      <Field label={copy.channel} hint={copy.channelHint}><Select value={channel} onChange={(event) => setChannel(event.target.value)}>{settings.available_channels.map((option) => <option key={option.value} value={option.value}>{option.label}</option>)}</Select></Field>
      <div><SwitchRow title={copy.automatic} description={copy.automaticHint} checked={automatic} onChange={setAutomatic} /><SwitchRow title={copy.push} description={copy.pushHint} checked={push} onChange={setPush} /><SwitchRow title={copy.telemetry} description={copy.telemetryHint} checked={telemetry} onChange={setTelemetry} /></div>
    </div>
  </ManagementSheet>
}

export function optimisticUpdateStatus(status: UpdateStatus, action: 'apply' | 'rollback'): UpdateStatus {
  return {
    ...status,
    active_job_id: `starting-${action}`,
    status: 'updating',
    progress_key: action === 'rollback' ? 'updates.progress.rollback' : 'updates.progress.preparing',
    last_error: '',
  }
}

function SwitchRow({ title, description, checked, onChange }: { title: string; description: string; checked: boolean; onChange: (next: boolean) => void }) { return <div className="final-switch-row"><span><strong>{title}</strong><small>{description}</small></span><button type="button" role="switch" className="final-switch" aria-checked={checked} aria-label={title} onClick={() => onChange(!checked)} /></div> }
function progressPercent(key: string, running: boolean) { if (!running) return key.includes('done') ? 100 : 0; if (key.includes('health')) return 88; if (key.includes('restart')) return 72; if (key.includes('pull')) return 45; if (key.includes('prepar')) return 18; return 8 }
function progressText(copy: Copy, key: string) { if (key.includes('failed')) return copy.progressFailed; if (key.includes('rollback')) return copy.progressRollback; if (key.includes('done')) return copy.progressDone; if (key.includes('health')) return copy.progressHealth; if (key.includes('restart')) return copy.progressRestarting; if (key.includes('pull')) return copy.progressPulling; if (key.includes('prepar')) return copy.progressPreparing; if (key.includes('check')) return copy.progressChecking; return copy.progressIdle }
function formatDate(value: string, language: string) { const date = new Date(value); return Number.isNaN(date.valueOf()) ? value : new Intl.DateTimeFormat(language, { dateStyle: 'medium', timeStyle: 'short' }).format(date) }
