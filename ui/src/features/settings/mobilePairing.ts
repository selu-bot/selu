export interface MobilePairingExpiration {
  expiresAtMs: number | null;
  remainingMs: number;
  isExpired: boolean;
}

/**
 * Encodes the payload expected by the legacy mobile-app QR scanner.
 *
 * Keep the server URL and token in this in-memory payload only. Callers must not
 * place either value in navigation URLs, persistent storage, or logs.
 */
export function encodeMobilePairingPayload(server_url: string, token: string): string {
  return JSON.stringify({ url: server_url, token });
}

/**
 * Computes the pairing-code expiration state from explicit inputs so callers
 * can update countdowns without introducing storage or logging side effects.
 * Invalid expiration timestamps fail closed as expired.
 */
export function parseMobilePairingTimestamp(value: string): number {
  const sqliteUtc = /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}(?:\.\d+)?$/;
  return Date.parse(sqliteUtc.test(value) ? `${value.replace(" ", "T")}Z` : value);
}

export function getMobilePairingExpiration(
  expires_at: string,
  nowMs: number,
): MobilePairingExpiration {
  const expiresAtMs = parseMobilePairingTimestamp(expires_at);

  if (!Number.isFinite(expiresAtMs)) {
    return {
      expiresAtMs: null,
      remainingMs: 0,
      isExpired: true,
    };
  }

  return {
    expiresAtMs,
    remainingMs: Math.max(0, expiresAtMs - nowMs),
    isExpired: expiresAtMs <= nowMs,
  };
}
