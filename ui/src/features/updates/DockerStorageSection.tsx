import { useEffect, useId, useReducer } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { HardDrive, RefreshCw, ShieldCheck, Trash2 } from 'lucide-react'
import { ApiError } from '../../api'
import { defineTranslations, useLanguage, useTranslations } from '../../i18n'
import { useNotices, useQueryErrorNotice } from '../../notices'
import { Button, ConfirmDialog, Skeleton, StatusBadge } from '../../shared/ui'
import { ManagementSection, ManagementSheet } from '../management/Management'
import {
  updatesApi,
  type DockerStorageCleanupResult,
  type DockerStorageEntry,
  type DockerStorageStatus,
} from './api'
import './DockerStorageSection.css'

export const storageMessages = defineTranslations({
  title: 'Docker storage',
  description: 'Selu keeps the images your installed agents need and saves rollback versions for you. Only images that are no longer needed are offered for cleanup.',
  managed: 'Managed total',
  protected: 'Protected',
  removable: 'Safely removable',
  image: 'image',
  images: 'images',
  preview: 'Preview cleanup',
  cleanUp: 'Clean up {size}',
  close: 'Close preview',
  cancel: 'Cancel',
  retry: 'Try again',
  loading: 'Checking Docker storage…',
  loadError: 'Docker storage could not be checked',
  loadErrorBody: 'Your agents are still protected. Try refreshing the storage status.',
  ready: 'Safe cleanup available',
  allProtected: 'Everything is protected',
  updateActive: 'Update in progress',
  cleanupActive: 'Cleanup in progress',
  cleanupPaused: 'Cleanup paused',
  readyBody: 'Selu found space it can free without touching installed agents or rollback versions.',
  emptyBody: 'There is nothing safe to remove right now.',
  updateBody: 'Cleanup will be available after the system update finishes.',
  cleanupActiveBody: 'Storage totals will refresh when the current cleanup finishes.',
  blockedBody: 'Cleanup is temporarily unavailable. Your managed images remain protected.',
  dockerUnavailableBody: 'Docker storage is temporarily unavailable. Your managed images remain protected.',
  lastCleanup: 'Last cleanup: {date}',
  neverCleaned: 'No cleanup has been run yet.',
  previewTitle: 'Preview Docker cleanup',
  previewDescription: 'Review what Selu is keeping and what can be removed safely. Nothing is removed until you confirm.',
  previewBlockedTitle: 'Cleanup is not ready yet',
  previewTotals: 'Storage totals',
  managedImages: 'Images managed by Selu',
  noEntries: 'No managed Docker images were reported.',
  stateProtected: 'Protected',
  stateRemovable: 'Safe to remove',
  stateManaged: 'Managed',
  reasonInstalledAgent: 'Needed by an installed agent',
  reasonSystemCurrent: 'Needed by Selu right now',
  reasonRollback: 'Saved for a rollback version',
  reasonContainer: 'Still used by a Docker container',
  reasonSafetyPeriod: 'Kept briefly while Selu confirms it is no longer needed',
  reasonCleanupBlocked: 'Kept while Selu completes its safety check',
  reasonUnused: 'No longer needed by an installed agent or rollback version',
  reasonManaged: 'Managed and kept safe by Selu',
  cleanupTitle: 'Clean up Docker storage?',
  cleanupBody: 'Selu will remove {count} and free up to {size}. Installed agents and rollback versions will stay protected.',
  cleanupComplete: 'Docker storage cleaned up',
  cleanupCompleteBody: 'Selu safely removed {count} and freed {size}. Protected images were left untouched.',
  cleanupPartial: 'Some Docker storage was cleaned up',
  cleanupPartialBody: 'Selu safely removed {count} and freed {size}. Some images could not be removed safely, so Selu left them in place.',
  cleanupFailed: 'Docker storage was not cleaned up',
  cleanupPreviewChanged: 'The cleanup list changed. Open the preview and review the current images before trying again.',
  cleanupUpdateActive: 'Wait for the current system update to finish, then try again.',
  cleanupDockerUnavailable: 'Docker is not available right now. Check Docker, then refresh the storage status.',
  cleanupBlockedBody: 'Selu could not confirm that cleanup is safe yet. Refresh the storage status and try again.',
}, {
  title: 'Docker-Speicher',
  description: 'Selu bewahrt die Images auf, die deine installierten Agenten brauchen, und hält Versionen für eine Rückkehr bereit. Zur Bereinigung werden nur Images angeboten, die nicht mehr benötigt werden.',
  managed: 'Insgesamt verwaltet',
  protected: 'Geschützt',
  removable: 'Sicher freigebbar',
  image: 'Image',
  images: 'Images',
  preview: 'Bereinigung ansehen',
  cleanUp: '{size} freigeben',
  close: 'Vorschau schließen',
  cancel: 'Abbrechen',
  retry: 'Erneut versuchen',
  loading: 'Docker-Speicher wird geprüft…',
  loadError: 'Docker-Speicher konnte nicht geprüft werden',
  loadErrorBody: 'Deine Agenten bleiben geschützt. Aktualisiere den Speicherstatus erneut.',
  ready: 'Sichere Bereinigung verfügbar',
  allProtected: 'Alles ist geschützt',
  updateActive: 'Aktualisierung läuft',
  cleanupActive: 'Bereinigung läuft',
  cleanupPaused: 'Bereinigung pausiert',
  readyBody: 'Selu hat Speicherplatz gefunden, der freigegeben werden kann, ohne installierte Agenten oder Rückkehrversionen anzutasten.',
  emptyBody: 'Im Moment kann nichts sicher entfernt werden.',
  updateBody: 'Die Bereinigung ist wieder verfügbar, sobald die Systemaktualisierung abgeschlossen ist.',
  cleanupActiveBody: 'Die Speicherwerte werden aktualisiert, sobald die laufende Bereinigung abgeschlossen ist.',
  blockedBody: 'Die Bereinigung ist vorübergehend nicht verfügbar. Deine verwalteten Images bleiben geschützt.',
  dockerUnavailableBody: 'Der Docker-Speicher ist vorübergehend nicht erreichbar. Deine verwalteten Images bleiben geschützt.',
  lastCleanup: 'Letzte Bereinigung: {date}',
  neverCleaned: 'Bisher wurde keine Bereinigung ausgeführt.',
  previewTitle: 'Docker-Bereinigung ansehen',
  previewDescription: 'Prüfe, was Selu aufbewahrt und was sicher entfernt werden kann. Vor deiner Bestätigung wird nichts entfernt.',
  previewBlockedTitle: 'Bereinigung ist noch nicht bereit',
  previewTotals: 'Speicherübersicht',
  managedImages: 'Von Selu verwaltete Images',
  noEntries: 'Es wurden keine verwalteten Docker-Images gemeldet.',
  stateProtected: 'Geschützt',
  stateRemovable: 'Sicher entfernbar',
  stateManaged: 'Verwaltet',
  reasonInstalledAgent: 'Wird von einem installierten Agenten benötigt',
  reasonSystemCurrent: 'Wird gerade von Selu benötigt',
  reasonRollback: 'Für eine Rückkehrversion aufbewahrt',
  reasonContainer: 'Wird noch von einem Docker-Container verwendet',
  reasonSafetyPeriod: 'Wird kurz aufbewahrt, während Selu prüft, ob es noch benötigt wird',
  reasonCleanupBlocked: 'Wird aufbewahrt, bis Selu die Sicherheitsprüfung abgeschlossen hat',
  reasonUnused: 'Wird von keinem installierten Agenten und keiner Rückkehrversion mehr benötigt',
  reasonManaged: 'Wird von Selu verwaltet und sicher aufbewahrt',
  cleanupTitle: 'Docker-Speicher bereinigen?',
  cleanupBody: 'Selu entfernt {count} und gibt bis zu {size} frei. Installierte Agenten und Rückkehrversionen bleiben geschützt.',
  cleanupComplete: 'Docker-Speicher bereinigt',
  cleanupCompleteBody: 'Selu hat {count} sicher entfernt und {size} freigegeben. Geschützte Images blieben unangetastet.',
  cleanupPartial: 'Ein Teil des Docker-Speichers wurde bereinigt',
  cleanupPartialBody: 'Selu hat {count} sicher entfernt und {size} freigegeben. Einige Images konnten nicht sicher entfernt werden und blieben deshalb erhalten.',
  cleanupFailed: 'Docker-Speicher wurde nicht bereinigt',
  cleanupPreviewChanged: 'Die Bereinigungsliste hat sich geändert. Öffne die Vorschau und prüfe die aktuellen Images, bevor du es erneut versuchst.',
  cleanupUpdateActive: 'Warte, bis die laufende Systemaktualisierung abgeschlossen ist, und versuche es dann erneut.',
  cleanupDockerUnavailable: 'Docker ist gerade nicht verfügbar. Prüfe Docker und aktualisiere danach den Speicherstatus.',
  cleanupBlockedBody: 'Selu konnte noch nicht bestätigen, dass die Bereinigung sicher ist. Aktualisiere den Speicherstatus und versuche es erneut.',
})

export type StorageCopy = { [K in keyof typeof storageMessages.en]: string }
export type StorageActivity = 'ready' | 'empty' | 'update' | 'cleanup' | 'blocked'
export type StorageUiState = {
  previewOpen: boolean
  previewSnapshotKey: string | null
  previewImageIds: string[]
  confirmCleanup: boolean
  confirmationSnapshotKey: string | null
  confirmedImageIds: string[]
}
export type StorageUiAction =
  | { type: 'open-preview'; snapshotKey: string; imageIds: string[] }
  | { type: 'close-preview' }
  | { type: 'request-cleanup' }
  | { type: 'snapshot-changed'; snapshotKey: string; ready: boolean }
  | { type: 'cancel-cleanup' }
  | { type: 'cleanup-success' }
  | { type: 'cleanup-error' }

const STORAGE_QUERY_KEY = ['system-updates', 'storage'] as const
const initialUiState: StorageUiState = {
  previewOpen: false,
  previewSnapshotKey: null,
  previewImageIds: [],
  confirmCleanup: false,
  confirmationSnapshotKey: null,
  confirmedImageIds: [],
}

export function cleanupCandidateImageIds(storage: DockerStorageStatus): string[] {
  return storage.entries
    .filter((entry) => entry.state === 'reclaimable')
    .map((entry) => entry.image_id)
}

export function storageSnapshotKey(storage: DockerStorageStatus): string {
  return JSON.stringify([
    storage.blocked_code,
    storage.blocked_reason,
    storage.managed_bytes,
    storage.protected_bytes,
    storage.reclaimable_bytes,
    storage.managed_image_count,
    storage.protected_image_count,
    storage.reclaimable_image_count,
    storage.last_cleanup_at,
    storage.entries.map((entry) => [entry.image_id, entry.state, entry.reason, entry.size_bytes]),
  ])
}

export function storageUiReducer(state: StorageUiState, action: StorageUiAction): StorageUiState {
  switch (action.type) {
    case 'open-preview':
      return {
        ...initialUiState,
        previewOpen: true,
        previewSnapshotKey: action.snapshotKey,
        previewImageIds: action.imageIds,
      }
    case 'close-preview':
    case 'cancel-cleanup':
    case 'cleanup-success':
    case 'cleanup-error':
      return initialUiState
    case 'request-cleanup':
      if (!state.previewOpen || !state.previewSnapshotKey || state.previewImageIds.length === 0) return state
      return {
        ...initialUiState,
        confirmCleanup: true,
        confirmationSnapshotKey: state.previewSnapshotKey,
        confirmedImageIds: state.previewImageIds,
      }
    case 'snapshot-changed': {
      const previewChanged = state.previewOpen && state.previewSnapshotKey !== action.snapshotKey
      const confirmationStale = state.confirmCleanup
        && (!action.ready || state.confirmationSnapshotKey !== action.snapshotKey)
      return previewChanged || confirmationStale ? initialUiState : state
    }
  }
}

export function getStorageActivity(storage: DockerStorageStatus, systemUpdateActive: boolean, cleanupActive: boolean): StorageActivity {
  if (cleanupActive || storage.blocked_code === 'cleanup_active') return 'cleanup'
  if (systemUpdateActive || storage.blocked_code === 'update_active') return 'update'
  if (storage.blocked_code) return 'blocked'
  return storage.reclaimable_bytes > 0 && storage.reclaimable_image_count > 0 ? 'ready' : 'empty'
}

export function DockerStorageSection({ systemUpdateActive }: { systemUpdateActive: boolean }) {
  const copy = useTranslations(storageMessages)
  const language = useLanguage()
  const notices = useNotices()
  const cache = useQueryClient()
  const [ui, dispatch] = useReducer(storageUiReducer, initialUiState)
  const storage = useQuery({
    queryKey: STORAGE_QUERY_KEY,
    queryFn: updatesApi.storage,
    retry: false,
    refetchInterval: 30_000,
  })
  useQueryErrorNotice(storage.error, copy.loadError)

  const cleanup = useMutation({
    mutationFn: updatesApi.cleanupStorage,
    onSuccess: (result) => {
      cache.setQueryData<DockerStorageStatus>(STORAGE_QUERY_KEY, result)
      dispatch({ type: 'cleanup-success' })
      const notice = storageCleanupNotice(copy, result, language)
      notices.success(notice.title, notice.body)
      void Promise.all([
        cache.invalidateQueries({ queryKey: STORAGE_QUERY_KEY }),
        cache.invalidateQueries({ queryKey: ['system-updates', 'status'] }),
      ])
    },
    onError: (error) => {
      dispatch({ type: 'cleanup-error' })
      notices.notify({ kind: 'error', title: copy.cleanupFailed, body: storageCleanupErrorBody(copy, error) })
      void cache.invalidateQueries({ queryKey: STORAGE_QUERY_KEY })
    },
  })

  const snapshot = storage.isError ? undefined : storage.data
  const snapshotKey = snapshot ? storageSnapshotKey(snapshot) : ''
  const serverActivity = snapshot ? getStorageActivity(snapshot, systemUpdateActive, false) : 'blocked'
  useEffect(() => {
    dispatch({ type: 'snapshot-changed', snapshotKey, ready: serverActivity === 'ready' })
  }, [serverActivity, snapshotKey])

  const openPreview = () => {
    if (!snapshot) return
    dispatch({
      type: 'open-preview',
      snapshotKey,
      imageIds: cleanupCandidateImageIds(snapshot),
    })
  }
  const requestCleanup = () => {
    if (snapshot && serverActivity === 'ready') dispatch({ type: 'request-cleanup' })
  }
  const confirmationCurrent = Boolean(snapshot)
    && ui.confirmationSnapshotKey === snapshotKey
    && serverActivity === 'ready'

  return <>
    <ManagementSection title={copy.title} description={copy.description}>
      {storage.isPending ? <DockerStorageLoading copy={copy} /> : !snapshot ? <DockerStorageError copy={copy} refreshing={storage.isFetching} onRetry={() => void storage.refetch()} /> : <DockerStorageSummary
        storage={snapshot}
        copy={copy}
        language={language}
        systemUpdateActive={systemUpdateActive}
        cleanupActive={cleanup.isPending}
        refreshing={storage.isFetching}
        onPreview={openPreview}
      />}
    </ManagementSection>
    {snapshot && <StoragePreviewSheet
      open={ui.previewOpen}
      storage={snapshot}
      copy={copy}
      language={language}
      activity={getStorageActivity(snapshot, systemUpdateActive, cleanup.isPending)}
      onClose={() => dispatch({ type: 'close-preview' })}
      onCleanup={requestCleanup}
    />}
    <ConfirmDialog
      open={ui.confirmCleanup && confirmationCurrent}
      title={copy.cleanupTitle}
      message={snapshot ? cleanupConfirmation(copy, snapshot, language) : ''}
      confirmLabel={snapshot ? cleanupLabel(copy, snapshot.reclaimable_bytes, language) : copy.cleanUp.replace('{size}', '0 B')}
      cancelLabel={copy.cancel}
      destructive
      busy={cleanup.isPending}
      onCancel={() => dispatch({ type: 'cancel-cleanup' })}
      onConfirm={() => {
        if (confirmationCurrent && !cleanup.isPending) cleanup.mutate(ui.confirmedImageIds)
      }}
    />
  </>
}

export function DockerStorageSummary({ storage, copy, language, systemUpdateActive, cleanupActive, refreshing = false, onPreview }: {
  storage: DockerStorageStatus
  copy: StorageCopy
  language: string
  systemUpdateActive: boolean
  cleanupActive: boolean
  refreshing?: boolean
  onPreview: () => void
}) {
  const statusId = useId()
  const activity = getStorageActivity(storage, systemUpdateActive, cleanupActive)
  const disabled = activity !== 'ready'
  const status = storageStatus(copy, storage, activity, language)
  return <div className="docker-storage-summary" aria-busy={refreshing || undefined}>
    <dl className="docker-storage-metrics">
      <StorageMetric label={copy.managed} bytes={storage.managed_bytes} count={storage.managed_image_count} copy={copy} language={language} />
      <StorageMetric label={copy.removable} bytes={storage.reclaimable_bytes} count={storage.reclaimable_image_count} copy={copy} language={language} emphasis />
    </dl>
    <div className="docker-storage-toolbar">
      <div className="docker-storage-status" role="status" aria-live="polite">
        <StatusBadge tone={status.tone}>{status.label}</StatusBadge>
        <p id={statusId}>{status.detail}</p>
      </div>
      <div className="docker-storage-actions">
        <Button leadingIcon={<ShieldCheck />} disabled={storage.entries.length === 0} onClick={onPreview}>{copy.preview}</Button>
        <Button variant="primary" leadingIcon={<Trash2 />} disabled={disabled} aria-describedby={statusId} onClick={onPreview}>{cleanupLabel(copy, storage.reclaimable_bytes, language)}</Button>
      </div>
    </div>
  </div>
}

function DockerStorageLoading({ copy }: { copy: StorageCopy }) {
  return <div className="docker-storage-loading" role="status" aria-live="polite" aria-busy="true">
    <HardDrive aria-hidden="true" />
    <span>{copy.loading}</span>
    <Skeleton width="42%" height={12} />
  </div>
}

function DockerStorageError({ copy, refreshing, onRetry }: { copy: StorageCopy; refreshing: boolean; onRetry: () => void }) {
  return <div className="docker-storage-error" role="alert">
    <div><strong>{copy.loadError}</strong><p>{copy.loadErrorBody}</p></div>
    <Button leadingIcon={<RefreshCw />} loading={refreshing} onClick={onRetry}>{copy.retry}</Button>
  </div>
}

function StorageMetric({ label, bytes, count, copy, language, emphasis = false }: {
  label: string
  bytes: number
  count: number
  copy: StorageCopy
  language: string
  emphasis?: boolean
}) {
  return <div className={`docker-storage-metric${emphasis ? ' is-emphasis' : ''}`}>
    <dt>{label}</dt>
    <dd>{formatStorageBytes(bytes, language)}</dd>
    <small>{formatImageCount(count, copy, language)}</small>
  </div>
}

function StoragePreviewSheet({ open, storage, copy, language, activity, onClose, onCleanup }: {
  open: boolean
  storage: DockerStorageStatus
  copy: StorageCopy
  language: string
  activity: StorageActivity
  onClose: () => void
  onCleanup: () => void
}) {
  return <ManagementSheet
    open={open}
    title={copy.previewTitle}
    description={copy.previewDescription}
    closeLabel={copy.close}
    onClose={onClose}
    actions={<><Button onClick={onClose}>{copy.close}</Button><Button variant="primary" leadingIcon={<Trash2 />} disabled={activity !== 'ready'} onClick={onCleanup}>{cleanupLabel(copy, storage.reclaimable_bytes, language)}</Button></>}
  >
    <StoragePreviewContent storage={storage} copy={copy} language={language} activity={activity} />
  </ManagementSheet>
}

export function StoragePreviewContent({ storage, copy, language, activity = getStorageActivity(storage, false, false) }: {
  storage: DockerStorageStatus
  copy: StorageCopy
  language: string
  activity?: StorageActivity
}) {
  const blockedStatus = activity === 'ready' || activity === 'empty'
    ? null
    : storageStatus(copy, storage, activity, language)
  return <div className="docker-storage-preview">
    {blockedStatus && <div className="docker-storage-preview-blocked" role="status" aria-live="polite">
      <strong>{copy.previewBlockedTitle}</strong>
      <StatusBadge tone={blockedStatus.tone}>{blockedStatus.label}</StatusBadge>
      <p>{blockedStatus.detail}</p>
    </div>}
    <section aria-labelledby="docker-storage-preview-totals">
      <h3 id="docker-storage-preview-totals">{copy.previewTotals}</h3>
      <dl className="docker-storage-preview-totals">
        <StorageMetric label={copy.managed} bytes={storage.managed_bytes} count={storage.managed_image_count} copy={copy} language={language} />
        <StorageMetric label={copy.protected} bytes={storage.protected_bytes} count={storage.protected_image_count} copy={copy} language={language} />
        <StorageMetric label={copy.removable} bytes={storage.reclaimable_bytes} count={storage.reclaimable_image_count} copy={copy} language={language} emphasis />
      </dl>
    </section>
    <section aria-labelledby="docker-storage-preview-images">
      <h3 id="docker-storage-preview-images">{copy.managedImages}</h3>
      {storage.entries.length === 0 ? <p className="management-muted">{copy.noEntries}</p> : <ul className="docker-storage-list">
        {storage.entries.map((entry) => <StorageEntryRow key={entry.image_id} entry={entry} copy={copy} language={language} />)}
      </ul>}
    </section>
  </div>
}

function StorageEntryRow({ entry, copy, language }: { entry: DockerStorageEntry; copy: StorageCopy; language: string }) {
  const removable = isRemovableState(entry.state)
  return <li className="docker-storage-entry">
    <div className="docker-storage-entry-copy">
      <strong>{entry.display_name || entry.image_id}</strong>
      <small>{storageReasonLabel(entry, copy)}</small>
    </div>
    <div className="docker-storage-entry-meta">
      <StatusBadge tone={removable ? 'info' : 'success'}>{storageStateLabel(entry.state, copy)}</StatusBadge>
      <span>{formatStorageBytes(entry.size_bytes, language)}</span>
    </div>
  </li>
}

export function storageStateLabel(state: DockerStorageEntry['state'], copy: StorageCopy): string {
  return state === 'reclaimable' ? copy.stateRemovable : copy.stateProtected
}

export function storageReasonLabel(entry: Pick<DockerStorageEntry, 'reason'>, copy: StorageCopy): string {
  switch (entry.reason) {
    case 'current_revision':
      return copy.reasonInstalledAgent
    case 'current_system_image':
      return copy.reasonSystemCurrent
    case 'previous_revision':
    case 'uninstall_retention':
    case 'staged_or_failed_revision':
    case 'superseded_retention':
    case 'retention_period':
      return copy.reasonRollback
    case 'container_in_use':
      return copy.reasonContainer
    case 'initial_grace_period':
      return copy.reasonSafetyPeriod
    case 'cleanup_blocked':
      return copy.reasonCleanupBlocked
    case 'unreferenced':
      return copy.reasonUnused
    default:
      return copy.reasonManaged
  }
}

export function storageCleanupNotice(copy: StorageCopy, result: DockerStorageCleanupResult, language: string): { title: string; body: string } {
  const partial = result.blocked_code === 'cleanup_incomplete'
  return {
    title: partial ? copy.cleanupPartial : copy.cleanupComplete,
    body: interpolate(partial ? copy.cleanupPartialBody : copy.cleanupCompleteBody, {
      count: formatImageCount(result.reclaimed_image_count, copy, language),
      size: formatStorageBytes(result.reclaimed_bytes, language),
    }),
  }
}

export function storageCleanupErrorBody(copy: StorageCopy, error: unknown): string {
  const code = error instanceof ApiError ? error.code : undefined
  if (code === 'preview_changed') return copy.cleanupPreviewChanged
  if (code === 'update_active') return copy.cleanupUpdateActive
  if (code === 'docker_unavailable') return copy.cleanupDockerUnavailable
  return copy.cleanupBlockedBody
}

export function formatStorageBytes(value: number, language: string): string {
  const bytes = Number.isFinite(value) ? Math.max(0, value) : 0
  if (bytes < 1024) return `${Math.round(bytes)} B`
  const units = ['KB', 'MB', 'GB', 'TB']
  let size = bytes / 1024
  let unit = units[0]
  for (let index = 1; index < units.length && size >= 1024; index += 1) {
    size /= 1024
    unit = units[index]
  }
  return `${new Intl.NumberFormat(language, { maximumFractionDigits: size < 10 ? 1 : 0 }).format(size)} ${unit}`
}

function cleanupLabel(copy: StorageCopy, bytes: number, language: string) {
  return copy.cleanUp.replace('{size}', formatStorageBytes(bytes, language))
}

function cleanupConfirmation(copy: StorageCopy, storage: DockerStorageStatus, language: string) {
  return interpolate(copy.cleanupBody, {
    count: formatImageCount(storage.reclaimable_image_count, copy, language),
    size: formatStorageBytes(storage.reclaimable_bytes, language),
  })
}

function formatImageCount(count: number, copy: StorageCopy, language: string) {
  const formatted = new Intl.NumberFormat(language).format(count)
  return `${formatted} ${count === 1 ? copy.image : copy.images}`
}

function storageStatus(copy: StorageCopy, storage: DockerStorageStatus, activity: StorageActivity, language: string): { label: string; detail: string; tone: 'success' | 'warning' | 'info' | 'neutral' } {
  if (activity === 'cleanup') return { label: copy.cleanupActive, detail: copy.cleanupActiveBody, tone: 'info' }
  if (activity === 'update') return { label: copy.updateActive, detail: copy.updateBody, tone: 'warning' }
  if (activity === 'blocked') return {
    label: copy.cleanupPaused,
    detail: storage.blocked_code === 'docker_unavailable' ? copy.dockerUnavailableBody : copy.blockedBody,
    tone: 'warning',
  }
  if (activity === 'empty') return { label: copy.allProtected, detail: copy.emptyBody, tone: 'success' }
  return {
    label: copy.ready,
    detail: storage.last_cleanup_at ? copy.lastCleanup.replace('{date}', formatStorageDate(storage.last_cleanup_at, language)) : copy.neverCleaned,
    tone: 'info',
  }
}

function formatStorageDate(value: string, language: string) {
  const date = new Date(value)
  return Number.isNaN(date.valueOf()) ? value : new Intl.DateTimeFormat(language, { dateStyle: 'medium', timeStyle: 'short' }).format(date)
}

function isRemovableState(state: DockerStorageEntry['state']) {
  return state === 'reclaimable'
}

function interpolate(template: string, values: Record<string, string>) {
  return Object.entries(values).reduce((result, [key, value]) => result.replace(`{${key}}`, value), template)
}
