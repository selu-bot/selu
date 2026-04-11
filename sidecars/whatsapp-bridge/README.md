# Selu WhatsApp Bridge (Baileys)

This sidecar connects WhatsApp to Selu using [Baileys](https://github.com/WhiskeySockets/Baileys).

## What it does

- Connects to WhatsApp Web (QR login).
- Receives inbound WhatsApp messages and forwards them to Selu's pipe inbound endpoint.
- Receives outbound messages from Selu and sends them to WhatsApp.

## API

- `GET /health` - health + connection state
- `GET /session/status` - connection + QR availability
- `GET /session/qr` - current QR code as data URL (when login is required)
- `GET /chats` - known chats/senders observed by the bridge (`?q=` filter supported)
- `POST /session/reconnect` - reconnect Baileys
- `POST /webhooks/selu/outbound` - Selu outbound webhook target
  - accepts `text` and optional `attachments[]`
  - `image/*` attachments are sent as WhatsApp images (via `download_url` or `data_base64`)
  - non-image attachments are sent as WhatsApp documents (including PDFs)

## Required environment variables

- `SELU_INBOUND_URL` - Selu pipe inbound URL (`.../api/pipes/<pipe_id>/inbound`)
- `SELU_INBOUND_TOKEN` - bearer token for the inbound URL

## Optional environment variables

- `BRIDGE_EXPECT_AUTH` - expected `Authorization` header value for outbound webhook calls
- `PORT` - default `3200`
- `BAILEYS_AUTH_DIR` - default `/data/auth`
- `BAILEYS_ALLOW_FROM_ME` - defaults to `false`; set `true` only if you intentionally want Selu to process messages sent by the logged-in account itself
- `LOG_LEVEL` - default `info`

## Storage

Persist `/data` as a Docker volume so WhatsApp session auth survives restarts.
