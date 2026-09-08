import { useEffect, useState, type FormEvent } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Brain, Clock3, Pencil, Plus, Trash2, UserRound } from 'lucide-react'
import { defineTranslations, setLanguage, useLanguage, useTranslations } from '../../i18n'
import { useNotices, useQueryErrorNotice } from '../../notices'
import { Button, ConfirmDialog, EmptyState, Field, Input, PageHeader, StatusBadge, Textarea } from '../../shared/ui'
import { AppPageShell } from '../shell/AppPageShell'
import { ManagementLoading, ManagementSection, ManagementSheet, OverviewCard, OverviewGrid } from '../management/Management'
import { aboutApi, type FactInput, type ProfileFact, type UserAccount } from './api'
import './AboutPage.css'

const messages = defineTranslations({
  eyebrow: 'Your space', title: 'About you', description: 'Help Selu understand what matters to you. You stay in control of everything saved here.', editProfile: 'Edit profile', profile: 'Your profile', profileDescription: 'How Selu addresses you and handles dates and times.', remembered: 'What Selu remembers', rememberedDescription: 'Useful details you have shared with Selu.', addFact: 'Add a detail', noFacts: 'Nothing saved yet', noFactsBody: 'Add a preference or detail whenever you want Selu to remember it.', displayName: 'Your name', language: 'Language', timezone: 'Timezone', timezoneHint: 'For example: Europe/Berlin or America/New_York', english: 'English', german: 'German', save: 'Save changes', cancel: 'Cancel', close: 'Close panel', profileSaved: 'Your profile was updated', fact: 'What should Selu remember?', category: 'Group', categoryHint: 'Optional — for example: preferences, family, or work', addTitle: 'Add a detail', editTitle: 'Edit this detail', factSaved: 'Saved to About you', deleteTitle: 'Remove this detail?', deleteBody: 'Selu will stop using this detail. This cannot be undone.', remove: 'Remove', removed: 'Detail removed', required: 'Please enter something for Selu to remember.', loadError: 'About you could not be loaded', saveError: 'Your changes were not saved', personal: 'Personal', updated: 'Updated {date}', accountSince: 'Using Selu since {date}', administrator: 'Administrator', member: 'Member', source: 'Saved by {source}',
}, {
  eyebrow: 'Dein Bereich', title: 'Über dich', description: 'Hilf Selu zu verstehen, was dir wichtig ist. Du behältst die Kontrolle über alles, was hier gespeichert ist.', editProfile: 'Profil bearbeiten', profile: 'Dein Profil', profileDescription: 'Wie Selu dich anspricht und mit Datum und Uhrzeit umgeht.', remembered: 'Was Selu über dich weiß', rememberedDescription: 'Hilfreiche Details, die du mit Selu geteilt hast.', addFact: 'Detail hinzufügen', noFacts: 'Noch nichts gespeichert', noFactsBody: 'Füge eine Vorliebe oder ein Detail hinzu, das Selu sich merken soll.', displayName: 'Dein Name', language: 'Sprache', timezone: 'Zeitzone', timezoneHint: 'Zum Beispiel: Europe/Berlin oder America/New_York', english: 'Englisch', german: 'Deutsch', save: 'Änderungen speichern', cancel: 'Abbrechen', close: 'Bereich schließen', profileSaved: 'Dein Profil wurde aktualisiert', fact: 'Was soll Selu sich merken?', category: 'Gruppe', categoryHint: 'Optional – zum Beispiel Vorlieben, Familie oder Arbeit', addTitle: 'Detail hinzufügen', editTitle: 'Dieses Detail bearbeiten', factSaved: 'Unter „Über dich“ gespeichert', deleteTitle: 'Dieses Detail entfernen?', deleteBody: 'Selu verwendet dieses Detail danach nicht mehr. Das kann nicht rückgängig gemacht werden.', remove: 'Entfernen', removed: 'Detail entfernt', required: 'Bitte gib ein, was Selu sich merken soll.', loadError: '„Über dich“ konnte nicht geladen werden', saveError: 'Deine Änderungen wurden nicht gespeichert', personal: 'Persönlich', updated: 'Aktualisiert: {date}', accountSince: 'Selu verwendet seit {date}', administrator: 'Administrator', member: 'Mitglied', source: 'Gespeichert von {source}',
})

export function AboutPage() {
  const copy = useTranslations(messages)
  const language = useLanguage()
  const notices = useNotices()
  const cache = useQueryClient()
  const profile = useQuery({ queryKey: ['account', 'me'], queryFn: aboutApi.me })
  const facts = useQuery({ queryKey: ['profile-facts'], queryFn: aboutApi.facts })
  useQueryErrorNotice(profile.error ?? facts.error, copy.loadError)
  const [profileOpen, setProfileOpen] = useState(false)
  const [factEditor, setFactEditor] = useState<ProfileFact | 'new' | null>(null)
  const [deleting, setDeleting] = useState<ProfileFact | null>(null)

  const refreshFacts = () => cache.invalidateQueries({ queryKey: ['profile-facts'] })
  const saveProfile = useMutation({
    mutationFn: aboutApi.updateMe,
    onSuccess: (user) => { cache.setQueryData(['account', 'me'], user); setLanguage(user.language === 'de' ? 'de' : 'en'); setProfileOpen(false); notices.success(copy.profileSaved) },
    onError: (error) => notices.error(error, copy.saveError),
  })
  const saveFact = useMutation({
    mutationFn: async ({ current, input }: { current: ProfileFact | 'new'; input: FactInput }) => { if (current === 'new') await aboutApi.createFact(input); else await aboutApi.updateFact(current.id, input) },
    onSuccess: () => { setFactEditor(null); void refreshFacts(); notices.success(copy.factSaved) },
    onError: (error) => notices.error(error, copy.saveError),
  })
  const removeFact = useMutation({
    mutationFn: (id: string) => aboutApi.deleteFact(id),
    onSuccess: () => { setDeleting(null); void refreshFacts(); notices.success(copy.removed) },
    onError: (error) => notices.error(error, copy.saveError),
  })

  return <AppPageShell active="about-you" width="wide">
    <PageHeader eyebrow={copy.eyebrow} title={copy.title} description={copy.description} actions={<Button variant="primary" leadingIcon={<Pencil />} onClick={() => setProfileOpen(true)}>{copy.editProfile}</Button>} />
    {profile.isPending || facts.isPending ? <ManagementLoading cards={2} /> : <>
      {profile.data && <OverviewGrid>
        <OverviewCard icon={<UserRound />} status={<StatusBadge tone={profile.data.is_admin ? 'info' : 'neutral'}>{profile.data.is_admin ? copy.administrator : copy.member}</StatusBadge>} title={profile.data.display_name} description={`@${profile.data.username}`} meta={copy.accountSince.replace('{date}', formatDate(profile.data.created_at, language))} actions={<Button size="sm" onClick={() => setProfileOpen(true)}>{copy.editProfile}</Button>} />
        <OverviewCard icon={<Clock3 />} title={profile.data.timezone} description={profile.data.language === 'de' ? copy.german : copy.english} meta={copy.profileDescription} />
      </OverviewGrid>}
      <ManagementSection title={copy.remembered} description={copy.rememberedDescription} actions={<Button size="sm" leadingIcon={<Plus />} onClick={() => setFactEditor('new')}>{copy.addFact}</Button>}>
        {facts.data?.length ? <div className="about-facts">{facts.data.map((item) => <article className="about-fact" key={item.id}><div><span>{item.category || copy.personal}</span><p>{item.fact}</p><small>{copy.updated.replace('{date}', formatDate(item.updated_at, language))}</small></div><div className="about-fact-actions"><Button size="sm" variant="ghost" onClick={() => setFactEditor(item)} leadingIcon={<Pencil />}>{copy.editTitle}</Button><Button size="sm" variant="ghost" onClick={() => setDeleting(item)} leadingIcon={<Trash2 />}>{copy.remove}</Button></div></article>)}</div> : <EmptyState icon={<Brain />} title={copy.noFacts} description={copy.noFactsBody} action={<Button variant="primary" leadingIcon={<Plus />} onClick={() => setFactEditor('new')}>{copy.addFact}</Button>} />}
      </ManagementSection>
    </>}
    {profile.data && <ProfileSheet user={profile.data} open={profileOpen} copy={copy} onClose={() => setProfileOpen(false)} onSave={(input) => saveProfile.mutate(input)} busy={saveProfile.isPending} />}
    <FactSheet value={factEditor} copy={copy} onClose={() => setFactEditor(null)} onSave={(input) => factEditor && saveFact.mutate({ current: factEditor, input })} busy={saveFact.isPending} />
    <ConfirmDialog open={Boolean(deleting)} title={copy.deleteTitle} message={copy.deleteBody} confirmLabel={copy.remove} cancelLabel={copy.cancel} destructive busy={removeFact.isPending} onCancel={() => setDeleting(null)} onConfirm={() => deleting && removeFact.mutate(deleting.id)} />
  </AppPageShell>
}

type Copy = { [K in keyof typeof messages.en]: string }
function ProfileSheet({ user, open, copy, onClose, onSave, busy }: { user: UserAccount; open: boolean; copy: Copy; onClose: () => void; onSave: (input: { display_name: string; language: 'en' | 'de'; timezone: string }) => void; busy: boolean }) {
  const [name, setName] = useState(user.display_name), [locale, setLocale] = useState<'en' | 'de'>(user.language === 'de' ? 'de' : 'en'), [timezone, setTimezone] = useState(user.timezone)
  useEffect(() => { if (open) { setName(user.display_name); setLocale(user.language === 'de' ? 'de' : 'en'); setTimezone(user.timezone) } }, [open, user])
  return <ManagementSheet open={open} title={copy.editProfile} description={copy.profileDescription} closeLabel={copy.close} onClose={onClose} busy={busy} actions={<><Button onClick={onClose} disabled={busy}>{copy.cancel}</Button><Button variant="primary" loading={busy} onClick={() => onSave({ display_name: name.trim(), language: locale, timezone: timezone.trim() })}>{copy.save}</Button></>}><div className="management-form"><Field label={copy.displayName}><Input value={name} onChange={(event) => setName(event.target.value)} maxLength={120} /></Field><Field label={copy.language}><select className="selu-ui-control selu-ui-select" value={locale} onChange={(event) => setLocale(event.target.value as 'en' | 'de')}><option value="en">{copy.english}</option><option value="de">{copy.german}</option></select></Field><Field label={copy.timezone} hint={copy.timezoneHint}><Input value={timezone} onChange={(event) => setTimezone(event.target.value)} maxLength={64} /></Field></div></ManagementSheet>
}

function FactSheet({ value, copy, onClose, onSave, busy }: { value: ProfileFact | 'new' | null; copy: Copy; onClose: () => void; onSave: (input: FactInput) => void; busy: boolean }) {
  const [fact, setFact] = useState(''), [category, setCategory] = useState(''), [error, setError] = useState('')
  useEffect(() => { if (value) { setFact(value === 'new' ? '' : value.fact); setCategory(value === 'new' ? '' : value.category); setError('') } }, [value])
  const submit = (event: FormEvent) => { event.preventDefault(); if (!fact.trim()) { setError(copy.required); return } onSave({ fact: fact.trim(), category: category.trim() || undefined }) }
  return <ManagementSheet open={Boolean(value)} title={value === 'new' ? copy.addTitle : copy.editTitle} closeLabel={copy.close} onClose={onClose} busy={busy} actions={<><Button onClick={onClose} disabled={busy}>{copy.cancel}</Button><Button variant="primary" loading={busy} onClick={() => document.getElementById('about-fact-form')?.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))}>{copy.save}</Button></>}><form id="about-fact-form" className="management-form" onSubmit={submit}><Field label={copy.fact} error={error}><Textarea rows={5} value={fact} onChange={(event) => { setFact(event.target.value); setError('') }} maxLength={2000} /></Field><Field label={copy.category} hint={copy.categoryHint}><Input value={category} onChange={(event) => setCategory(event.target.value)} maxLength={80} /></Field></form></ManagementSheet>
}
function formatDate(value: string, language: string) { const date = new Date(value); return Number.isNaN(date.valueOf()) ? '' : new Intl.DateTimeFormat(language, { dateStyle: 'medium' }).format(date) }
