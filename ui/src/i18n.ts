import { useSyncExternalStore } from 'react'

export const translations = {
  en: {
    agents: 'Agents', backToConversations: 'Back to conversations', chat: 'Chat', closeNavigation: 'Close navigation',
    collapseNavigation: 'Collapse navigation', connectors: 'Messaging', connections: 'Connections', conversationOptions: 'Conversation options',
    conversations: 'Conversations', copied: 'Copied', copy: 'Copy', addPhotos: 'Add photos', selectedPhotos: 'Selected photos', removePhoto: 'Remove photo', photosNotAdded: 'Those photos couldn’t be added', photoAttachments: 'Photos', openPhoto: 'Open photo',
    darkMode: 'Dark mode', expandNavigation: 'Expand navigation', feedback: 'Feedback', followingAlong: 'You can follow along',
    gettingReady: 'Getting everything ready', jumpToLatest: 'Jump to latest', lightMode: 'Light mode', live: 'Live',
    loading: 'Loading', mainNavigation: 'Main navigation', manage: 'Manage', memory: 'About you', mobileApp: 'Mobile app',
    newConversation: 'New conversation', noConversations: 'A fresh start', nothingFound: 'Nothing found',
    openNavigation: 'Open navigation', people: 'People', personalAgent: 'Personal agent', placeholder: 'What can Selu handle for you?',
    privateByDesign: 'Private by design', providers: 'AI services', ready: 'Ready', schedules: 'Automations', searchConversations: 'Find a conversation',
    send: 'Send', sendHint: 'Enter to send · Shift + Enter for a new line', settings: 'Settings', updates: 'System updates', somethingWentWrong: 'That didn’t work',
    startConversation: 'Start a conversation', startConversationHint: 'Start one whenever you’re ready.', stepsAvailable: 'Open to see what happened',
    technicalDetails: 'Technical details', theme: 'Theme', tryAgain: 'Please try again.', tryAnotherSearch: 'Try a different word or phrase.',
    waitForReply: 'Selu is still working…', welcomeBody: 'Ask naturally. Selu can think, use your connected tools, and keep you in the loop without getting in your way.',
    welcomeTitle: 'What shall we take care of?', workDone: 'Work completed', working: 'Working', workspace: 'Workspace',
    advanced: 'Advanced', allow: 'Allow', approvalDetails: 'What Selu will use', approvalNeeded: 'Your okay is needed',
    cache: 'Caches', credentials: 'Credentials', deny: 'Not now', language: 'Language', signOut: 'Sign out',
    you: 'You', yourPersonalAgent: 'Your personal agent', yourSpace: 'Your space',
    cancel: 'Cancel', deleteConversation: 'Delete conversation', deleteConversationBody: 'This removes the conversation and everything Selu produced in it. This cannot be undone.',
    deleteWhileWorking: 'Wait until Selu has finished before deleting this conversation.', loadMore: 'Show older conversations', loadingMore: 'Loading…',
    rename: 'Rename', renameConversation: 'Rename conversation', save: 'Save', schedule: 'Schedule', scheduledRuns: 'Scheduled runs',
    scheduleHint: 'Selu posts the results of this schedule here. You can reply to follow up.', titlePlaceholder: 'Give this conversation a name',
    dismiss: 'Dismiss', errorApprovalExpired: 'That request has already expired. Selu will ask again if it still needs your okay.',
    errorNotAvailable: 'This action isn’t available right now. Selu may need a restart or an update.',
    errorRunInProgress: 'Selu is still working on this conversation. Wait a moment and try again.',
    errorInvalidPhoto: 'That photo couldn’t be read. Choose a JPEG, PNG, GIF, or WebP image.',
    errorPhotoTooLarge: 'Keep each photo under 2 MB and the whole selection under 12 MB.',
    errorTooManyPhotos: 'You can send up to 10 photos at a time.',
    errorPhotoCommand: 'Photos can’t be sent with a command. Remove the photos or send a normal message.',
    errorServer: 'Selu ran into a problem on its side. Please try again in a moment.', errorSessionExpired: 'You were signed out. Please sign in again.',
    approvalDenied: 'Okay, Selu will skip that step.', approvalGranted: 'Thanks, Selu is continuing.', conversationDeleted: 'Conversation deleted',
    conversationRenamed: 'Conversation renamed', errorInvalidTitle: 'Please enter a name for the conversation.', errorOffline: 'Selu couldn’t be reached. Check your connection and try again.',
    messageNotSent: 'Your message wasn’t sent', notifications: 'Notifications',
    helpful: 'Helpful', notHelpful: 'Not helpful', feedbackNotSaved: 'Your feedback wasn’t saved',
    commands: 'Commands', commandsHint: 'Type / for commands',
    errorFeedbackUnavailable: 'This reply can’t be rated yet. Give Selu a moment and try again.',
    home: 'Home', today: 'Today', savedTopics: 'Saved topics', pastDays: 'Past days', allConversations: 'All conversations',
    todayWithSelu: 'Today with Selu', todayTimelineHint: 'Quick questions and scheduled results, together in the order they happened.',
    saveTopic: 'Save topic', savedTopic: 'Saved topic', nameTopic: 'Name this topic', nameTopicHint: 'Give it a name you will recognize later.',
    unsaveTopic: 'Remove from saved', topicSaved: 'Topic saved', topicRemoved: 'Removed from saved',
    noTodayActivity: 'Nothing here yet today. Ask Selu anything above.', noSavedTopics: 'Save an ordinary conversation to keep it here.',
    noPastActivity: 'No past activity matches your search.', pastDaysTitle: 'Past days', pastDaysHint: 'Search everything you and Selu have worked through.',
    searchPastDays: 'Search past days', searchSavedTopics: 'Search saved topics', upcomingAutomations: 'Coming up', nextRun: 'Next run', noUpcomingAutomations: 'No active automations are coming up.',
    scheduledResult: 'Scheduled result', openConversation: 'Open conversation',
    homeGreeting: 'What can I take care of, {name}?', homeGreetingFallback: 'What can I take care of?',
    homeSubtitle: 'Start with whatever is on your mind. Selu will help you move it forward.', homePlaceholder: 'Ask Selu anything…',
    homeComposerHint: 'Press Enter to send · Shift + Enter for a new line', needsAttention: 'In progress',
    needsAttentionHint: 'Things Selu is working on', nothingNeedsAttention: 'Nothing is running right now.', upcoming: 'Coming up',
    upcomingHint: 'Scheduled work', nothingUpcoming: 'Nothing scheduled right now.', recent: 'Recent', recentHint: 'Pick up where you left off',
    noRecentConversations: 'Your recent conversations will appear here.', welcomeBack: 'Welcome back', loginTitle: 'Sign in to Selu',
    loginBody: 'Continue where you left off.', firstRun: 'A fresh start', setupTitle: 'Make Selu yours',
    setupBody: 'Create the first account for this Selu.', displayName: 'Your name', username: 'Username', password: 'Password',
    showPassword: 'Show password', hidePassword: 'Hide password', signingIn: 'One moment…', signIn: 'Sign in', finishSetup: 'Finish setup',
    authPrivate: 'Your sign-in stays on this Selu.', loadingPage: 'Selu is getting this page ready.', pageNotFound: 'This page could not be found.', tryAgainAction: 'Try again',
  },
  de: {
    agents: 'Agenten', backToConversations: 'Zurück zu Unterhaltungen', chat: 'Chat', closeNavigation: 'Navigation schließen',
    collapseNavigation: 'Navigation einklappen', connectors: 'Nachrichten', connections: 'Verbindungen', conversationOptions: 'Optionen der Unterhaltung',
    conversations: 'Unterhaltungen', copied: 'Kopiert', copy: 'Kopieren', addPhotos: 'Fotos hinzufügen', selectedPhotos: 'Ausgewählte Fotos', removePhoto: 'Foto entfernen', photosNotAdded: 'Diese Fotos konnten nicht hinzugefügt werden', photoAttachments: 'Fotos', openPhoto: 'Foto öffnen',
    darkMode: 'Dunkler Modus', expandNavigation: 'Navigation ausklappen', feedback: 'Feedback', followingAlong: 'Du kannst dabei zusehen',
    gettingReady: 'Ich bereite alles vor', jumpToLatest: 'Zum neuesten Beitrag', lightMode: 'Heller Modus', live: 'Live',
    loading: 'Wird geladen', mainNavigation: 'Hauptnavigation', manage: 'Verwalten', memory: 'Über dich', mobileApp: 'Mobile App',
    newConversation: 'Neue Unterhaltung', noConversations: 'Ein frischer Anfang', nothingFound: 'Nichts gefunden',
    openNavigation: 'Navigation öffnen', people: 'Personen', personalAgent: 'Persönlicher Agent', placeholder: 'Was kann Selu für dich erledigen?',
    privateByDesign: 'Von Grund auf privat', providers: 'KI-Dienste', ready: 'Bereit', schedules: 'Automatisierungen', searchConversations: 'Unterhaltung finden',
    send: 'Senden', sendHint: 'Enter zum Senden · Umschalt + Enter für eine neue Zeile', settings: 'Einstellungen', updates: 'Systemaktualisierungen', somethingWentWrong: 'Das hat nicht geklappt',
    startConversation: 'Unterhaltung beginnen', startConversationHint: 'Beginne, sobald du bereit bist.', stepsAvailable: 'Öffnen, um den Ablauf zu sehen',
    technicalDetails: 'Technische Details', theme: 'Darstellung', tryAgain: 'Versuche es bitte noch einmal.', tryAnotherSearch: 'Versuche ein anderes Wort oder eine andere Formulierung.',
    waitForReply: 'Selu arbeitet noch…', welcomeBody: 'Frag einfach natürlich. Selu kann nachdenken, deine verbundenen Werkzeuge nutzen und dich auf dem Laufenden halten.',
    welcomeTitle: 'Worum kümmern wir uns?', workDone: 'Arbeit abgeschlossen', working: 'Ich kümmere mich', workspace: 'Arbeitsbereich',
    advanced: 'Erweitert', allow: 'Erlauben', approvalDetails: 'Was Selu verwenden wird', approvalNeeded: 'Deine Zustimmung ist nötig',
    cache: 'Caches', credentials: 'Zugangsdaten', deny: 'Jetzt nicht', language: 'Sprache', signOut: 'Abmelden',
    you: 'Du', yourPersonalAgent: 'Dein persönlicher Agent', yourSpace: 'Dein Bereich',
    cancel: 'Abbrechen', deleteConversation: 'Unterhaltung löschen', deleteConversationBody: 'Die Unterhaltung und alles, was Selu darin erstellt hat, werden entfernt. Das lässt sich nicht rückgängig machen.',
    deleteWhileWorking: 'Warte, bis Selu fertig ist, bevor du diese Unterhaltung löschst.', loadMore: 'Ältere Unterhaltungen anzeigen', loadingMore: 'Wird geladen…',
    rename: 'Umbenennen', renameConversation: 'Unterhaltung umbenennen', save: 'Speichern', schedule: 'Zeitplan', scheduledRuns: 'Geplante Läufe',
    scheduleHint: 'Selu veröffentlicht die Ergebnisse dieses Zeitplans hier. Du kannst darauf antworten.', titlePlaceholder: 'Gib dieser Unterhaltung einen Namen',
    dismiss: 'Schließen', errorApprovalExpired: 'Diese Anfrage ist bereits abgelaufen. Selu fragt erneut, falls deine Zustimmung noch nötig ist.',
    errorNotAvailable: 'Diese Aktion ist gerade nicht verfügbar. Selu braucht eventuell einen Neustart oder ein Update.',
    errorRunInProgress: 'Selu arbeitet noch an dieser Unterhaltung. Warte kurz und versuche es dann erneut.',
    errorInvalidPhoto: 'Dieses Foto konnte nicht gelesen werden. Wähle ein JPEG-, PNG-, GIF- oder WebP-Bild.',
    errorPhotoTooLarge: 'Jedes Foto darf höchstens 2 MB groß sein, die gesamte Auswahl höchstens 12 MB.',
    errorTooManyPhotos: 'Du kannst bis zu 10 Fotos auf einmal senden.',
    errorPhotoCommand: 'Fotos können nicht mit einem Befehl gesendet werden. Entferne die Fotos oder sende eine normale Nachricht.',
    errorServer: 'Bei Selu ist ein Problem aufgetreten. Versuche es bitte gleich noch einmal.', errorSessionExpired: 'Du wurdest abgemeldet. Bitte melde dich erneut an.',
    approvalDenied: 'Okay, Selu überspringt diesen Schritt.', approvalGranted: 'Danke, Selu macht weiter.', conversationDeleted: 'Unterhaltung gelöscht',
    conversationRenamed: 'Unterhaltung umbenannt', errorInvalidTitle: 'Bitte gib der Unterhaltung einen Namen.', errorOffline: 'Selu ist gerade nicht erreichbar. Prüfe deine Verbindung und versuche es erneut.',
    messageNotSent: 'Deine Nachricht wurde nicht gesendet', notifications: 'Benachrichtigungen',
    helpful: 'Hilfreich', notHelpful: 'Nicht hilfreich', feedbackNotSaved: 'Dein Feedback wurde nicht gespeichert',
    commands: 'Befehle', commandsHint: 'Tippe / für Befehle',
    errorFeedbackUnavailable: 'Diese Antwort kann noch nicht bewertet werden. Gib Selu einen Moment und versuche es erneut.',
    home: 'Start', today: 'Heute', savedTopics: 'Gespeicherte Themen', pastDays: 'Frühere Tage', allConversations: 'Alle Unterhaltungen',
    todayWithSelu: 'Heute mit Selu', todayTimelineHint: 'Kurze Fragen und geplante Ergebnisse – gemeinsam in zeitlicher Reihenfolge.',
    saveTopic: 'Thema speichern', savedTopic: 'Gespeichertes Thema', nameTopic: 'Thema benennen', nameTopicHint: 'Gib ihm einen Namen, den du später wiedererkennst.',
    unsaveTopic: 'Aus Gespeichert entfernen', topicSaved: 'Thema gespeichert', topicRemoved: 'Aus Gespeichert entfernt',
    noTodayActivity: 'Heute ist hier noch nichts. Frag Selu einfach oben.', noSavedTopics: 'Speichere eine normale Unterhaltung, damit sie hier bleibt.',
    noPastActivity: 'Keine früheren Aktivitäten passen zu deiner Suche.', pastDaysTitle: 'Frühere Tage', pastDaysHint: 'Durchsuche alles, was du mit Selu bearbeitet hast.',
    searchPastDays: 'Frühere Tage durchsuchen', searchSavedTopics: 'Gespeicherte Themen durchsuchen', upcomingAutomations: 'Demnächst', nextRun: 'Nächster Lauf', noUpcomingAutomations: 'Keine aktiven Automatisierungen stehen an.',
    scheduledResult: 'Geplantes Ergebnis', openConversation: 'Unterhaltung öffnen',
    homeGreeting: 'Worum kann ich mich kümmern, {name}?', homeGreetingFallback: 'Worum kann ich mich kümmern?',
    homeSubtitle: 'Beginne mit dem, was dich gerade beschäftigt. Selu hilft dir, es voranzubringen.', homePlaceholder: 'Frag Selu einfach…',
    homeComposerHint: 'Enter zum Senden · Umschalt + Enter für eine neue Zeile', needsAttention: 'In Arbeit',
    needsAttentionHint: 'Aufgaben, an denen Selu gerade arbeitet', nothingNeedsAttention: 'Im Moment läuft nichts.', upcoming: 'Demnächst',
    upcomingHint: 'Geplante Aufgaben', nothingUpcoming: 'Im Moment ist nichts geplant.', recent: 'Zuletzt', recentHint: 'Mach dort weiter, wo du aufgehört hast',
    noRecentConversations: 'Deine letzten Unterhaltungen erscheinen hier.', welcomeBack: 'Willkommen zurück', loginTitle: 'Bei Selu anmelden',
    loginBody: 'Mach dort weiter, wo du aufgehört hast.', firstRun: 'Ein frischer Anfang', setupTitle: 'Mach Selu zu deinem',
    setupBody: 'Erstelle das erste Konto für dieses Selu.', displayName: 'Dein Name', username: 'Benutzername', password: 'Passwort',
    showPassword: 'Passwort anzeigen', hidePassword: 'Passwort ausblenden', signingIn: 'Einen Moment…', signIn: 'Anmelden', finishSetup: 'Einrichtung abschließen',
    authPrivate: 'Deine Anmeldung bleibt auf diesem Selu.', loadingPage: 'Selu bereitet diese Seite vor.', pageNotFound: 'Diese Seite wurde nicht gefunden.', tryAgainAction: 'Erneut versuchen',
  },
} as const

export type TranslationKey = keyof typeof translations.en
export type Language = keyof typeof translations
const storage = typeof localStorage === 'undefined' ? null : localStorage
const browserLanguage = typeof navigator === 'undefined' ? 'en' : navigator.language
let language: Language = storage?.getItem('selu.language') === 'de' || (!storage?.getItem('selu.language') && browserLanguage.toLowerCase().startsWith('de')) ? 'de' : 'en'
if (typeof document !== 'undefined') document.documentElement.lang = language
export const t = (key: TranslationKey) => translations[language][key]
export const getLanguage = () => language

const languageListeners = new Set<() => void>()
const subscribeLanguage = (listener: () => void) => {
  languageListeners.add(listener)
  return () => languageListeners.delete(listener)
}

export const useLanguage = () => useSyncExternalStore(subscribeLanguage, getLanguage, getLanguage)

export type TranslationBundle<T extends Record<string, string>> = {
  en: T
  de: { [K in keyof T]: string }
}

export function defineTranslations<const T extends Record<string, string>>(
  en: T,
  de: { [K in keyof T]: string },
): TranslationBundle<T> {
  return { en, de }
}

export function useTranslations<T extends Record<string, string>>(bundle: TranslationBundle<T>): { [K in keyof T]: string } {
  return bundle[useLanguage()]
}

export const setLanguage = (next: Language) => {
  if (language === next) return
  language = next
  storage?.setItem('selu.language', next)
  if (typeof document !== 'undefined') document.documentElement.lang = next
  languageListeners.forEach((listener) => listener())
}
