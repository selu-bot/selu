import { useEffect, useRef, useState, type FormEvent } from 'react'
import { useMutation } from '@tanstack/react-query'
import { Bug, CircleHelp, Heart, Lightbulb, MessageSquare, Send } from 'lucide-react'
import { defineTranslations, useTranslations } from '../../i18n'
import { useNotices } from '../../notices'
import { Button, Field, Input, PageHeader, Textarea } from '../../shared/ui'
import { AppPageShell } from '../shell/AppPageShell'
import { ManagementSheet, OverviewCard, OverviewGrid } from '../management/Management'
import { feedbackApi, type FeedbackInput, type FeedbackReceipt } from './api'
import './FeedbackPage.css'

const messages = defineTranslations({
  eyebrow: 'Help shape Selu', title: 'Feedback', description: 'Found something confusing, have an idea, or need help? Tell the people building Selu.', bug: 'Something is broken', bugBody: 'Tell us what happened and what you expected instead.', idea: 'I have an idea', ideaBody: 'Share a change that would make Selu more useful to you.', question: 'I need help', questionBody: 'Ask something you could not answer in the app or documentation.', other: 'Something else', otherBody: 'Share anything that does not fit the other choices.', start: 'Write feedback', panelTitle: 'Send feedback', panelDescription: 'Your message becomes a public GitHub issue so the Selu team can follow up.', publicWarning: 'Do not include passwords, private conversations, personal details, or other sensitive information.', shortTitle: 'Short summary', shortTitleHint: 'Optional, up to 100 characters', details: 'What would you like us to know?', detailsHint: 'Between 10 and 2,000 characters', send: 'Send feedback', cancel: 'Cancel', close: 'Close panel', required: 'Please write at least a few words and keep the message under 2,000 characters.', sent: 'Thank you — your feedback was sent', sentBody: 'The Selu team can now review it. You can follow its progress on GitHub.', view: 'View feedback', sendAnother: 'Send more feedback', failed: 'Your feedback was not sent',
}, {
  eyebrow: 'Gestalte Selu mit', title: 'Feedback', description: 'Ist etwas unklar, hast du eine Idee oder brauchst du Hilfe? Sag es den Menschen, die Selu entwickeln.', bug: 'Etwas funktioniert nicht', bugBody: 'Beschreibe, was passiert ist und was du stattdessen erwartet hast.', idea: 'Ich habe eine Idee', ideaBody: 'Teile eine Änderung, die Selu für dich nützlicher machen würde.', question: 'Ich brauche Hilfe', questionBody: 'Stelle eine Frage, die App oder Dokumentation nicht beantwortet haben.', other: 'Etwas anderes', otherBody: 'Teile alles, was nicht zu den anderen Möglichkeiten passt.', start: 'Feedback schreiben', panelTitle: 'Feedback senden', panelDescription: 'Deine Nachricht wird zu einem öffentlichen GitHub-Issue, damit das Selu-Team sie bearbeiten kann.', publicWarning: 'Gib keine Passwörter, privaten Unterhaltungen, persönlichen Angaben oder andere vertrauliche Informationen an.', shortTitle: 'Kurze Zusammenfassung', shortTitleHint: 'Optional, höchstens 100 Zeichen', details: 'Was möchtest du uns mitteilen?', detailsHint: 'Zwischen 10 und 2.000 Zeichen', send: 'Feedback senden', cancel: 'Abbrechen', close: 'Bereich schließen', required: 'Schreibe bitte ein paar Wörter und bleibe unter 2.000 Zeichen.', sent: 'Danke – dein Feedback wurde gesendet', sentBody: 'Das Selu-Team kann es jetzt prüfen. Den Fortschritt kannst du auf GitHub verfolgen.', view: 'Feedback ansehen', sendAnother: 'Weiteres Feedback senden', failed: 'Dein Feedback wurde nicht gesendet',
})
type Category = FeedbackInput['category']
type Copy = { [K in keyof typeof messages.en]: string }
const categories: { id: Category; icon: typeof Bug; title: keyof Copy; body: keyof Copy }[] = [{ id: 'bug', icon: Bug, title: 'bug', body: 'bugBody' }, { id: 'idea', icon: Lightbulb, title: 'idea', body: 'ideaBody' }, { id: 'question', icon: CircleHelp, title: 'question', body: 'questionBody' }, { id: 'other', icon: MessageSquare, title: 'other', body: 'otherBody' }]

export function FeedbackPage() {
  const copy = useTranslations(messages), notices = useNotices(), [category, setCategory] = useState<Category | null>(null), [receipt, setReceipt] = useState<FeedbackReceipt | null>(null)
  const submit = useMutation({ mutationFn: feedbackApi.submit, onSuccess: setReceipt, onError: (error) => notices.error(error, copy.failed) })
  const close = () => { if (!submit.isPending) { setCategory(null); setReceipt(null) } }
  return <AppPageShell active="feedback" width="wide"><PageHeader eyebrow={copy.eyebrow} title={copy.title} description={copy.description} />
    <OverviewGrid>{categories.map(({ id, icon: Icon, title, body }) => <OverviewCard key={id} icon={<Icon />} title={copy[title]} description={copy[body]} actions={<Button size="sm" onClick={() => setCategory(id)}>{copy.start}</Button>} />)}</OverviewGrid>
    <FeedbackSheet category={category} receipt={receipt} copy={copy} busy={submit.isPending} onClose={close} onSubmit={(input) => submit.mutate(input)} onAgain={() => setReceipt(null)} />
  </AppPageShell>
}
function FeedbackSheet({ category, receipt, copy, busy, onClose, onSubmit, onAgain }: { category: Category | null; receipt: FeedbackReceipt | null; copy: Copy; busy: boolean; onClose: () => void; onSubmit: (input: FeedbackInput) => void; onAgain: () => void }) {
  const form = useRef<HTMLFormElement>(null), [title, setTitle] = useState(''), [description, setDescription] = useState(''), [error, setError] = useState('')
  useEffect(() => { if (category) { setTitle(''); setDescription(''); setError('') } }, [category])
  const submit = (event: FormEvent) => { event.preventDefault(); const message = description.trim(); if (message.length < 10 || message.length > 2000 || title.length > 100 || !category) { setError(copy.required); return } onSubmit({ category, title: title.trim() || undefined, description: message }) }
  const issueUrl = receipt ? safeIssueUrl(receipt.issue_url) : null
  return <ManagementSheet open={Boolean(category)} title={receipt ? copy.sent : copy.panelTitle} description={receipt ? copy.sentBody : copy.panelDescription} closeLabel={copy.close} onClose={onClose} busy={busy} actions={receipt ? <><Button onClick={onAgain}>{copy.sendAnother}</Button>{issueUrl && <a className="selu-ui-button is-primary" href={issueUrl} target="_blank" rel="noreferrer">{copy.view}</a>}</> : <><Button onClick={onClose}>{copy.cancel}</Button><Button variant="primary" loading={busy} leadingIcon={<Send />} onClick={() => form.current?.requestSubmit()}>{copy.send}</Button></>}>
    {receipt ? <div className="feedback-success"><Heart /><p>{copy.sentBody}</p><span>#{receipt.issue_number}</span></div> : <form ref={form} className="management-form" onSubmit={submit}><div className="management-inline-error">{copy.publicWarning}</div>{error && <div className="management-inline-error">{error}</div>}<Field label={copy.shortTitle} hint={copy.shortTitleHint}><Input value={title} onChange={(event) => setTitle(event.target.value)} maxLength={100} /></Field><Field label={copy.details} hint={copy.detailsHint}><Textarea value={description} onChange={(event) => { setDescription(event.target.value); setError('') }} rows={9} maxLength={2000} /></Field></form>}
  </ManagementSheet>
}
export function safeIssueUrl(value: string) { try { const url = new URL(value); return url.protocol === 'https:' ? url.toString() : null } catch { return null } }
