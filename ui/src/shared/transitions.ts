type TransitionDocument = Document & {
  startViewTransition?: (update: () => void | Promise<void>) => { finished: Promise<void> }
}

export function prefersReducedMotion(match = (query: string) => window.matchMedia(query)): boolean {
  return match('(prefers-reduced-motion: reduce)').matches
}

export async function navigateWithTransition(update: () => void | Promise<void>): Promise<void> {
  const page = document as TransitionDocument
  if (prefersReducedMotion() || !page.startViewTransition) {
    await update()
    return
  }
  await page.startViewTransition(update).finished
}
