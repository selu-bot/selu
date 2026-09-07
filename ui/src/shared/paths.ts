const PLACEHOLDER = '__SELU_BASE_PATH__'

export function normalizeBasePath(value: string | null | undefined): string {
  if (!value || value === '/' || value.includes(PLACEHOLDER)) return ''
  const normalized = `/${value}`.replace(/\/{2,}/g, '/').replace(/\/$/, '')
  return normalized === '/' ? '' : normalized
}

export function getBasePath(): string {
  return normalizeBasePath(document.querySelector('meta[name="selu-base-path"]')?.getAttribute('content'))
}

export function appPath(path: string, basePath = getBasePath()): string {
  const suffix = path.startsWith('/') ? path : `/${path}`
  return `${basePath}${suffix}`
}
