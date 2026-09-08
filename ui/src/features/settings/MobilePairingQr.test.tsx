import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, describe, expect, it, vi } from "vitest";

import { MobilePairingQr, type MobilePairingQrLabels } from "./MobilePairingQr";
import {
  encodeMobilePairingPayload,
  getMobilePairingExpiration,
} from "./mobilePairing";

const labels: MobilePairingQrLabels = {
  qrCode: "Scan this QR code with the Selu mobile app",
  expiresIn: "Expires in",
  expired: "This pairing code has expired.",
  regenerate: "Generate a new code",
  regenerating: "Generating…",
};

const NOW = Date.parse("2026-09-08T09:00:00.000Z");
const EXPIRES_AT = "2026-09-08T09:05:00.000Z";

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe("encodeMobilePairingPayload", () => {
  it("preserves the exact legacy url and token JSON schema", () => {
    const payload = encodeMobilePairingPayload(
      "https://selu.example",
      "pairing-token",
    );

    expect(payload).toBe(
      '{"url":"https://selu.example","token":"pairing-token"}',
    );
    expect(Object.keys(JSON.parse(payload))).toEqual(["url", "token"]);
    expect(JSON.parse(payload)).toEqual({
      url: "https://selu.example",
      token: "pairing-token",
    });
  });

  it("serializes special characters without changing either value", () => {
    const serverUrl =
      'https://selu.example/app?next=%2Fchat&label=quote"\\line&emoji=👻';
    const token = 'token"\\with\nnewlines\tand\u0000controls/+/=';

    const payload = encodeMobilePairingPayload(serverUrl, token);

    expect(payload).toBe(JSON.stringify({ url: serverUrl, token }));
    expect(payload).not.toContain("\n");
    expect(JSON.parse(payload)).toEqual({ url: serverUrl, token });
  });
});

describe("getMobilePairingExpiration", () => {
  it("parses the backend's timezone-less SQLite timestamp as UTC", () => {
    expect(getMobilePairingExpiration("2026-09-08 09:05:00", NOW)).toEqual({
      expiresAtMs: Date.parse(EXPIRES_AT),
      remainingMs: 300_000,
      isExpired: false,
    });
  });

  it("treats the exact expiry instant as expired and clamps elapsed time", () => {
    const expiresAtMs = Date.parse(EXPIRES_AT);

    expect(getMobilePairingExpiration(EXPIRES_AT, expiresAtMs - 1)).toEqual({
      expiresAtMs,
      remainingMs: 1,
      isExpired: false,
    });
    expect(getMobilePairingExpiration(EXPIRES_AT, expiresAtMs)).toEqual({
      expiresAtMs,
      remainingMs: 0,
      isExpired: true,
    });
    expect(getMobilePairingExpiration(EXPIRES_AT, expiresAtMs + 1)).toEqual({
      expiresAtMs,
      remainingMs: 0,
      isExpired: true,
    });
  });

  it("fails closed for an invalid expiry without scheduling-dependent state", () => {
    expect(getMobilePairingExpiration("not-a-timestamp", NOW)).toEqual({
      expiresAtMs: null,
      remainingMs: 0,
      isExpired: true,
    });
  });
});

describe("MobilePairingQr", () => {
  it("renders an SVG QR without exposing the token as a link", () => {
    vi.useFakeTimers();
    vi.setSystemTime(NOW);
    const token = 'private-token-"-\\-👻';

    const html = renderToStaticMarkup(
      createElement(MobilePairingQr, {
        serverUrl: "https://selu.example",
        token,
        expiresAt: EXPIRES_AT,
        labels,
        onRegenerate: vi.fn(),
      }),
    );

    expect(html).toContain("<svg");
    expect(html).toContain('role="img"');
    expect(html).toContain(labels.qrCode);
    expect(html).not.toContain("<a");
    expect(html).not.toContain("href=");
    expect(html).not.toContain(token);
    expect(html).toContain("5:00");
  });

  it("replaces an expired QR with a regeneration control", () => {
    vi.useFakeTimers();
    vi.setSystemTime(NOW);

    const html = renderToStaticMarkup(
      createElement(MobilePairingQr, {
        serverUrl: "https://selu.example",
        token: "expired-fixture-token",
        expiresAt: new Date(NOW).toISOString(),
        labels,
        onRegenerate: vi.fn(),
      }),
    );

    expect(html).not.toContain("<svg");
    expect(html).toContain(labels.expired);
    expect(html).toContain(labels.regenerate);
    expect(html).not.toContain("expired-fixture-token");
  });
});
