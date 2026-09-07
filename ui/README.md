# Selu UI

The React SPA is Selu's complete browser application. The orchestrator serves its production build at `/app/`; every page reads and mutates state through authenticated, versioned JSON APIs. Old management GET URLs are compatibility redirects only.

## Product architecture

- `src/features/` contains each page and its typed API module. Keep page-specific server state in TanStack Query and reusable presentation in shared components.
- `src/components/` contains the application shell, navigation, conversation components, and focused reusable interface pieces.
- `src/shared/ui/` contains accessible controls shared across feature pages.
- `src/api.ts` owns the common same-origin client and shared session/conversation contracts. Feature API modules build on it and always include the HttpOnly session cookie.
- `src/i18n.ts` contains shared English and German copy; feature pages use colocated `defineTranslations(...)` bundles with compile-time language parity.
- `src/style.css` and feature stylesheets use the shared design tokens. Light, dark, narrow-screen, and reduced-motion behavior must remain first-class.
- Lucide is the shared icon system. The Selu companion in `BrandMark.tsx` is a product asset and intentionally does not come from the icon library.

The SPA shell is public so it can render login, setup, and expired-session states. Protected data and mutations require the normal Selu session through `/api/v1`; administrator operations enforce administrator access server-side. The Rust server applies a restrictive Content Security Policy plus clickjacking, MIME-sniffing, referrer, and cache protections. Never put tokens in browser storage or add third-party scripts, analytics, or remote fonts.

## Feedback pattern

Every outcome the user cannot see directly goes through one channel:
`src/notices.tsx`. Call `useNotices()` and use `success`, `info`, or `error`.
Notices stack top-right, auto-dismiss (4 s success, 6 s info, 8 s error), can
always be closed, are announced to screen readers, and never block the UI.

Rules for choosing the right feedback:

- **Ongoing work** stays on the element that started it: a disabled button, the
  disabled composer, the presence pill. Never use a notice for "loading".
- **Validation** is inline, next to the field. Disable the primary action until
  the input is valid instead of reporting afterwards.
- **Confirmation** of a destructive or irreversible action is a dialog
  (`ConversationActions.tsx` shows the pattern). If the dialog's own action
  fails, render the error inside the dialog via `describeError(error).body` and
  skip the notice so the message appears where the user acted.
- **Success** of an action whose result is not obvious on screen gets a
  `success` notice: deleting a conversation (it vanishes from the list), renaming
  it, or anything that happened on another screen. Skip it when the result is
  already visible, such as a sent message appearing in the thread.
- **Errors** from mutations are reported in the mutation's `onError`. Query
  errors use `useQueryErrorNotice(error)`, which fires once per failure instead
  of on every render. Always pass the raw error: `describeError` maps API codes
  and HTTP statuses to plain-language copy in `i18n.ts` (`error*` keys). Add a
  key there for every new server error code instead of showing the code.
- **Information** the user did not ask for (a decision was recorded, something
  changed elsewhere) is an `info` notice, kept to one sentence.

Titles say what happened ("Conversation deleted"), bodies say what to do next.
Both must exist in English and German before the change is complete.

## Local development

Run `npm install` once, then `npm run dev`. Docker builds the production assets
in a Node build stage and copies them into the orchestrator image.

For an IDE run of the Rust binary, build the SPA once with `npm run build`.
The server automatically reads `ui/dist` from this checkout. Open
`http://localhost:3000/app/` (the trailing slash is intentional); use
`SELU__UI_DIR` only when you want to point the server at a different build.

The Vite development server only serves the frontend. To use real chats, run the
orchestrator as well and configure Vite to reach the same origin, or use the
production-style `npm run build` workflow above.

## Release checks

Run `npm run build` for type-checking and the optimized bundle. Test keyboard
navigation, English and German, light and dark themes, narrow phone widths,
stream reconnection, tool-detail disclosure, and `prefers-reduced-motion` before
shipping a UI change.
