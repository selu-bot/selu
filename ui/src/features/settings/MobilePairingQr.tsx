import React, { useEffect, useMemo, useState } from "react";
import { QRCodeSVG } from "qrcode.react";

import {
  encodeMobilePairingPayload,
  getMobilePairingExpiration,
  type MobilePairingExpiration,
} from "./mobilePairing";

const COUNTDOWN_INTERVAL_MS = 1_000;
const QR_CODE_SIZE_PX = 224;
const QR_QUIET_ZONE_MODULES = 4;

export interface MobilePairingQrLabels {
  qrCode: string;
  expiresIn: string;
  expired: string;
  regenerate: string;
  regenerating: string;
}

export interface MobilePairingQrProps {
  serverUrl: string;
  token: string;
  expiresAt: string;
  labels: MobilePairingQrLabels;
  onRegenerate: () => void | Promise<void>;
  isRegenerating?: boolean;
}

function readExpiration(expiresAt: string): MobilePairingExpiration {
  return getMobilePairingExpiration(expiresAt, Date.now());
}

function formatRemainingTime(remainingMs: number): string {
  const totalSeconds = Math.ceil(remainingMs / 1_000);
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;

  return `${minutes}:${seconds.toString().padStart(2, "0")}`;
}

export function MobilePairingQr({
  serverUrl,
  token,
  expiresAt,
  labels,
  onRegenerate,
  isRegenerating = false,
}: MobilePairingQrProps) {
  const payload = useMemo(
    () => encodeMobilePairingPayload(serverUrl, token),
    [serverUrl, token],
  );
  const [expiration, setExpiration] = useState<MobilePairingExpiration>(() =>
    readExpiration(expiresAt),
  );

  useEffect(() => {
    const updateExpiration = () => {
      const nextExpiration = readExpiration(expiresAt);
      setExpiration(nextExpiration);
      return nextExpiration;
    };

    const initialExpiration = updateExpiration();
    if (initialExpiration.isExpired) {
      return undefined;
    }

    const timerId = window.setInterval(() => {
      const nextExpiration = updateExpiration();
      if (nextExpiration.isExpired) {
        window.clearInterval(timerId);
      }
    }, COUNTDOWN_INTERVAL_MS);

    return () => window.clearInterval(timerId);
  }, [expiresAt]);

  if (expiration.isExpired) {
    return (
      <div className="mobile-pairing-qr mobile-pairing-qr--expired">
        <p className="mobile-pairing-qr__expired" role="status">
          {labels.expired}
        </p>
        <button
          className="button button--secondary mobile-pairing-qr__regenerate"
          type="button"
          onClick={() => void onRegenerate()}
          disabled={isRegenerating}
        >
          {isRegenerating ? labels.regenerating : labels.regenerate}
        </button>
      </div>
    );
  }

  const remainingTime = formatRemainingTime(expiration.remainingMs);

  return (
    <div className="mobile-pairing-qr" data-testid="mobile-pairing-qr">
      <div className="mobile-pairing-qr__code">
        <QRCodeSVG
          value={payload}
          size={QR_CODE_SIZE_PX}
          level="M"
          marginSize={QR_QUIET_ZONE_MODULES}
          bgColor="#ffffff"
          fgColor="#000000"
          title={labels.qrCode}
          role="img"
          aria-label={labels.qrCode}
        />
      </div>
      <p className="mobile-pairing-qr__timer" role="timer">
        <span>{labels.expiresIn}</span>{" "}
        <time dateTime={expiresAt}>{remainingTime}</time>
      </p>
    </div>
  );
}
