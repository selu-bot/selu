const express = require('express');
const pino = require('pino');
const QRCode = require('qrcode');
const {
  default: makeWASocket,
  useMultiFileAuthState,
  fetchLatestBaileysVersion,
  DisconnectReason,
  downloadMediaMessage,
} = require('@whiskeysockets/baileys');

const logger = pino({ level: process.env.LOG_LEVEL || 'info' });

const PORT = parseInt(process.env.PORT || '3200', 10);
const AUTH_DIR = process.env.BAILEYS_AUTH_DIR || '/data/auth';
const BRIDGE_EXPECT_AUTH = (process.env.BRIDGE_EXPECT_AUTH || '').trim();
const SELU_INBOUND_URL = (process.env.SELU_INBOUND_URL || '').trim();
const SELU_INBOUND_TOKEN = (process.env.SELU_INBOUND_TOKEN || '').trim();
const BAILEYS_ALLOW_FROM_ME = (process.env.BAILEYS_ALLOW_FROM_ME || 'false').trim() === 'true';

let sock = null;
let latestQrDataUrl = null;
let connectionState = 'starting';
let lastError = null;
let currentJid = null;
const knownChats = new Map();
const recentMessageRefs = new Map();
const recentInboundMessages = new Map();
const recentOutboundMessages = new Map();
const recentProcessedInbound = new Map();

function digitsToPhoneJid(value) {
  const digits = (value || '').replace(/[^0-9]/g, '');
  if (!digits) return '';
  return `${digits}@s.whatsapp.net`;
}

function phoneUserToJid(value) {
  const raw = (value || '').trim();
  if (!raw) return '';
  const userPart = raw.split('@')[0] || '';
  const phonePart = userPart.split(':')[0] || '';
  return digitsToPhoneJid(phonePart);
}

function lidUserToJid(value) {
  const raw = (value || '').trim();
  if (!raw.endsWith('@lid')) return '';
  const userPart = raw.split('@')[0] || '';
  const lidPart = userPart.split(':')[0] || '';
  return lidPart ? `${lidPart}@lid` : '';
}

function collectWhatsAppRefs(value, refs = new Set(), depth = 0) {
  if (depth > 5 || value == null) return refs;

  if (typeof value === 'string') {
    const raw = value.trim();
    if (!raw) return refs;
    if (raw.includes('@')) refs.add(raw);

    const lidRef = lidUserToJid(raw);
    if (lidRef) refs.add(lidRef);

    const phoneRef = phoneUserToJid(raw);
    if (phoneRef) refs.add(phoneRef);
    return refs;
  }

  if (Array.isArray(value)) {
    for (const item of value) {
      collectWhatsAppRefs(item, refs, depth + 1);
    }
    return refs;
  }

  if (typeof value === 'object') {
    for (const key of Object.keys(value)) {
      collectWhatsAppRefs(value[key], refs, depth + 1);
    }
  }

  return refs;
}

function ownRefs() {
  return Array.from(collectWhatsAppRefs([currentJid, sock && sock.user ? sock.user : null]));
}

function ownPhoneJid() {
  const refs = ownRefs();
  return (
    refs.find((ref) => ref.endsWith('@s.whatsapp.net') && !ref.includes(':')) ||
    refs.find((ref) => ref.endsWith('@s.whatsapp.net')) ||
    ''
  );
}

function looksLikeDirectLidChat(ref) {
  const value = (ref || '').trim();
  return value.endsWith('@lid');
}

function isOwnRecipient(value) {
  const raw = (value || '').trim();
  return Boolean(raw) && ownRefs().includes(raw);
}

function ownChatJid() {
  const refs = ownRefs();
  return (
    refs.find((ref) => looksLikeDirectLidChat(ref) && !ref.includes(':')) ||
    refs.find((ref) => looksLikeDirectLidChat(ref)) ||
    ownPhoneJid()
  );
}

function selfRecipientCandidates() {
  const chatRef = ownChatJid();
  const phoneRef = ownPhoneJid();
  if (chatRef) return [chatRef];
  if (phoneRef) return [phoneRef];
  return [];
}

function normalizeRecipient(value) {
  const v = (value || '').trim();
  if (!v) return '';
  if (v.includes('@')) {
    // WhatsApp can report participant refs as @lid; outbound send often needs
    // the phone/chat JID. If we've seen this participant before, remap first.
    if (v.endsWith('@lid')) {
      const known = knownChats.get(v);
      if (known) {
        if (known.phone_ref) return known.phone_ref;
        if (known.chat_ref && !known.chat_ref.endsWith('@lid')) return known.chat_ref;
      }
    }
    return v;
  }
  return digitsToPhoneJid(v);
}

function recipientCandidates(value) {
  const raw = (value || '').trim();
  const primary = normalizeRecipient(raw);
  const out = [];

  if (raw.endsWith('@lid')) {
    const known = knownChats.get(raw);
    if (known) {
      if (known.phone_ref) out.push(known.phone_ref);
      if (known.chat_ref) out.push(known.chat_ref);
    }
    const phoneFallback = digitsToPhoneJid(raw);
    if (phoneFallback) out.push(phoneFallback);
  }

  if (primary) {
    out.push(primary);
  }

  if (raw && raw !== primary) {
    out.push(raw);
  }

  return Array.from(new Set(out));
}

function quotedRecipientCandidates(quoted, rawRecipient) {
  if (isOwnRecipient(rawRecipient)) {
    return selfRecipientCandidates();
  }

  const out = [];
  const quotedKey = quoted && quoted.key ? quoted.key : null;
  const remoteJid = quotedKey && quotedKey.remoteJid ? String(quotedKey.remoteJid).trim() : '';
  const participant = quotedKey && quotedKey.participant ? String(quotedKey.participant).trim() : '';

  if (remoteJid) out.push(remoteJid);
  if (participant) out.push(participant);

  return Array.from(new Set([...out, ...recipientCandidates(rawRecipient)]));
}

function extractText(msg) {
  if (!msg || !msg.message) return '';
  const m = msg.message;
  if (m.conversation) return m.conversation;
  if (m.extendedTextMessage && m.extendedTextMessage.text) return m.extendedTextMessage.text;
  if (m.imageMessage && m.imageMessage.caption) return m.imageMessage.caption;
  if (m.videoMessage && m.videoMessage.caption) return m.videoMessage.caption;
  return '';
}

function extractReplyToRef(msg) {
  const m = msg && msg.message;
  if (!m || !m.extendedTextMessage || !m.extendedTextMessage.contextInfo) return null;
  const info = m.extendedTextMessage.contextInfo;
  return info.stanzaId || null;
}

async function extractInboundAttachments(msg, sockInstance) {
  const out = [];
  const m = msg && msg.message ? msg.message : null;
  if (!m) return out;

  if (m.imageMessage) {
    const image = m.imageMessage;
    let dataBase64 = null;

    try {
      if (typeof downloadMediaMessage !== 'function') {
        throw new Error('Baileys downloadMediaMessage helper is unavailable');
      }
      const mediaBuffer = await downloadMediaMessage(
        msg,
        'buffer',
        {},
        {
          logger,
          reuploadRequest:
            sockInstance && typeof sockInstance.updateMediaMessage === 'function'
              ? sockInstance.updateMediaMessage
              : undefined,
        }
      );
      if (mediaBuffer && mediaBuffer.length) {
        dataBase64 = Buffer.from(mediaBuffer).toString('base64');
      }
    } catch (err) {
      logger.warn(
        {
          err,
          messageId: msg?.key?.id || null,
        },
        'Failed to download/decrypt WhatsApp image media'
      );
    }

    out.push({
      filename: `whatsapp-image-${msg?.key?.id || Date.now()}.jpg`,
      mime_type: (image.mimetype || 'image/jpeg').toString(),
      size_bytes: Number(image.fileLength || 0) || null,
      download_url: dataBase64 ? null : image.url ? String(image.url) : null,
      data_base64: dataBase64,
    });
  }

  return out;
}

function findPhoneJidDeep(value, depth = 0) {
  if (depth > 5 || value == null) return '';
  if (typeof value === 'string') {
    const s = value.trim();
    if (s.endsWith('@s.whatsapp.net')) return s;
    return '';
  }
  if (Array.isArray(value)) {
    for (const item of value) {
      const found = findPhoneJidDeep(item, depth + 1);
      if (found) return found;
    }
    return '';
  }
  if (typeof value === 'object') {
    for (const k of Object.keys(value)) {
      const found = findPhoneJidDeep(value[k], depth + 1);
      if (found) return found;
    }
  }
  return '';
}

function upsertKnownChat(senderRef, label, chatRef, phoneRef) {
  const sender = (senderRef || '').trim();
  if (!sender) return;
  const safeLabel = (label || '').trim() || sender;
  const existing = knownChats.get(sender);
  knownChats.set(sender, {
    sender_ref: sender,
    chat_ref: (chatRef || '').trim() || sender,
    phone_ref: (phoneRef || '').trim() || (existing && existing.phone_ref) || '',
    label: safeLabel,
    updated_at: Date.now(),
  });
}

function ensureOwnChatEntries() {
  const selfRefs = ownRefs();
  if (!selfRefs.length) return;

  const phoneRef = ownPhoneJid();
  const chatRef = ownChatJid() || phoneRef;

  for (const selfRef of selfRefs) {
    upsertKnownChat(selfRef, 'You', chatRef || selfRef, phoneRef || selfRef);
  }
}

function rememberMessageRefs(messageId, refs) {
  const id = (messageId || '').trim();
  if (!id) return [];

  const existing = recentMessageRefs.get(id) || { refs: new Set(), updated_at: 0 };
  for (const ref of refs) {
    const value = (ref || '').trim();
    if (value) existing.refs.add(value);
  }
  existing.updated_at = Date.now();
  recentMessageRefs.set(id, existing);

  for (const [key, value] of recentMessageRefs.entries()) {
    if (value.updated_at < Date.now() - 10 * 60 * 1000) {
      recentMessageRefs.delete(key);
    }
  }

  return Array.from(existing.refs);
}

function rememberInboundMessage(msg) {
  const id = msg && msg.key && msg.key.id ? String(msg.key.id).trim() : '';
  if (!id || !msg || !msg.message) return;

  recentInboundMessages.set(id, {
    key: msg.key,
    message: msg.message,
    pushName: msg.pushName || null,
    updated_at: Date.now(),
  });

  for (const [key, value] of recentInboundMessages.entries()) {
    if (value.updated_at < Date.now() - 24 * 60 * 60 * 1000) {
      recentInboundMessages.delete(key);
    }
  }
}

function shouldSkipDuplicateInbound(messageId) {
  const id = (messageId || '').trim();
  if (!id) return false;

  const lastSeen = recentProcessedInbound.get(id) || 0;
  const now = Date.now();
  recentProcessedInbound.set(id, now);

  for (const [key, value] of recentProcessedInbound.entries()) {
    if (value < now - 10 * 60 * 1000) {
      recentProcessedInbound.delete(key);
    }
  }

  return lastSeen > 0 && now - lastSeen < 30 * 1000;
}

function lookupQuotedMessage(messageRef) {
  const id = (messageRef || '').trim();
  if (!id) return null;
  return recentInboundMessages.get(id) || null;
}

function rememberOutboundMessage(messageId, details) {
  const id = (messageId || '').trim();
  if (!id) return;

  recentOutboundMessages.set(id, {
    ...(details || {}),
    updated_at: Date.now(),
  });

  for (const [key, value] of recentOutboundMessages.entries()) {
    if ((value.updated_at || 0) < Date.now() - 24 * 60 * 60 * 1000) {
      recentOutboundMessages.delete(key);
    }
  }
}

function whatsappStatusLabel(status) {
  switch (status) {
    case 0:
      return 'pending';
    case 1:
      return 'server_ack';
    case 2:
      return 'device_ack';
    case 3:
      return 'read';
    case 4:
      return 'played';
    default:
      return `unknown_${status}`;
  }
}

function mergeKnownChats(refs, label, preferredChatRef, preferredPhoneRef) {
  const allRefs = Array.from(
    new Set(
      (refs || [])
        .map((ref) => (ref || '').trim())
        .filter(Boolean)
    )
  );
  if (!allRefs.length) return;

  let canonicalPhoneRef = (preferredPhoneRef || '').trim();
  let canonicalChatRef = (preferredChatRef || '').trim();

  for (const ref of allRefs) {
    const known = knownChats.get(ref);
    if (!canonicalPhoneRef && known && known.phone_ref) canonicalPhoneRef = known.phone_ref;
    if (!canonicalChatRef && known && known.chat_ref) canonicalChatRef = known.chat_ref;
  }

  if (!canonicalPhoneRef) {
    canonicalPhoneRef = allRefs.find((ref) => ref.endsWith('@s.whatsapp.net')) || '';
  }

  if (!canonicalChatRef || canonicalChatRef.endsWith('@lid')) {
    canonicalChatRef =
      canonicalPhoneRef ||
      allRefs.find((ref) => ref.includes('@') && !ref.endsWith('@lid')) ||
      canonicalChatRef ||
      allRefs[0];
  }

  for (const ref of allRefs) {
    upsertKnownChat(ref, label, canonicalChatRef, canonicalPhoneRef);
  }
}

async function postInbound(payload) {
  if (!SELU_INBOUND_URL || !SELU_INBOUND_TOKEN) {
    logger.warn('SELU_INBOUND_URL or SELU_INBOUND_TOKEN not configured; dropping inbound');
    return;
  }

  const resp = await fetch(SELU_INBOUND_URL, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Authorization: `Bearer ${SELU_INBOUND_TOKEN}`,
    },
    body: JSON.stringify(payload),
  });

  if (!resp.ok) {
    const body = await resp.text().catch(() => '');
    logger.warn({ status: resp.status, body }, 'Selu inbound rejected message');
  }
}

async function startSocket() {
  const { state, saveCreds } = await useMultiFileAuthState(AUTH_DIR);
  const { version } = await fetchLatestBaileysVersion();

  sock = makeWASocket({
    version,
    auth: state,
    printQRInTerminal: false,
    browser: ['Selu WhatsApp Bridge', 'Chrome', '1.0.0'],
  });

  sock.ev.on('creds.update', saveCreds);

  sock.ev.on('connection.update', async (update) => {
    const { connection, lastDisconnect, qr } = update;

    if (qr) {
      latestQrDataUrl = await QRCode.toDataURL(qr);
      connectionState = 'qr_required';
      logger.info('WhatsApp QR updated');
    }

    if (connection === 'open') {
      connectionState = 'connected';
      latestQrDataUrl = null;
      lastError = null;
      currentJid = sock && sock.user ? sock.user.id : null;
      ensureOwnChatEntries();
      logger.info({ jid: currentJid }, 'WhatsApp connected');
    }

    if (connection === 'close') {
      const statusCode =
        lastDisconnect &&
        lastDisconnect.error &&
        lastDisconnect.error.output &&
        lastDisconnect.error.output.statusCode;

      const loggedOut = statusCode === DisconnectReason.loggedOut;
      connectionState = loggedOut ? 'logged_out' : 'reconnecting';
      lastError = loggedOut ? 'Session logged out. Reconnect and scan QR again.' : 'Connection lost';

      if (!loggedOut) {
        logger.warn('WhatsApp connection closed, reconnecting');
        setTimeout(() => {
          startSocket().catch((err) => {
            logger.error({ err }, 'Failed to restart socket');
            connectionState = 'error';
            lastError = 'Failed to restart WhatsApp connection';
          });
        }, 1000);
      }
    }
  });

  sock.ev.on('messages.update', (updates) => {
    try {
      if (!Array.isArray(updates)) return;
      for (const update of updates) {
        const key = update && update.key ? update.key : null;
        const messageId = key && key.id ? String(key.id) : '';
        if (!messageId) continue;

        const tracked = recentOutboundMessages.get(messageId) || null;
        const statusCode =
          update &&
          update.update &&
          typeof update.update.status === 'number'
            ? update.update.status
            : null;
        const elapsedMs =
          tracked && tracked.updated_at ? Date.now() - tracked.updated_at : null;
        const observedRemoteJid = key && key.remoteJid ? key.remoteJid : null;
        const remoteJidChanged =
          Boolean(tracked && tracked.remoteJid && observedRemoteJid) &&
          tracked.remoteJid !== observedRemoteJid;

        logger.info(
          {
            messageId,
            remoteJid: observedRemoteJid,
            fromMe: key ? Boolean(key.fromMe) : null,
            statusCode,
            statusLabel: statusCode == null ? null : whatsappStatusLabel(statusCode),
            elapsedMs,
            remoteJidChanged,
            trackedOutbound: tracked,
            update,
          },
          'WhatsApp message status updated'
        );

        if (remoteJidChanged) {
          logger.warn(
            {
              messageId,
              originalRemoteJid: tracked.remoteJid,
              observedRemoteJid,
              statusCode,
              statusLabel: statusCode == null ? null : whatsappStatusLabel(statusCode),
              elapsedMs,
            },
            'WhatsApp outbound message changed remote JID after send'
          );
        }
      }
    } catch (err) {
      logger.error({ err }, 'Failed to process WhatsApp message status updates');
    }
  });

  sock.ev.on('messages.upsert', async (event) => {
    try {
      if (!event || event.type !== 'notify' || !Array.isArray(event.messages)) return;

      for (const msg of event.messages) {
        if (!msg || !msg.key) continue;
        if (shouldSkipDuplicateInbound(msg.key.id || '')) {
          logger.info(
            {
              messageId: msg.key.id || null,
              chatRef: msg.key.remoteJid || null,
            },
            'Skipping duplicate WhatsApp inbound event'
          );
          continue;
        }

        const chatRef = (msg.key.remoteJid || '').trim();
        const participantRef = (msg.key.participant || '').trim();
        const phoneRef = findPhoneJidDeep(msg);
        const selfRefs = ownRefs();
        const isLikelySelfChatFromLid =
          Boolean(msg.key.fromMe) && looksLikeDirectLidChat(chatRef) && !participantRef;
        const isSelfChat =
          isLikelySelfChatFromLid ||
          selfRefs.length > 0 &&
          [chatRef, participantRef, phoneRef].some((ref) => selfRefs.includes((ref || '').trim()));

        if (msg.key.fromMe && !BAILEYS_ALLOW_FROM_ME && !isSelfChat) {
          logger.info(
            {
              messageId: msg.key.id || null,
              chatRef: chatRef || null,
              participantRef: participantRef || null,
              phoneRef: phoneRef || null,
              selfRefs,
            },
            'Skipping fromMe WhatsApp event'
          );
          continue;
        }

        const text = extractText(msg).trim();
        const inboundAttachments = await extractInboundAttachments(msg, sock);
        if (!text && inboundAttachments.length === 0) continue;

        const canonicalSelfPhoneRef = ownPhoneJid() || selfRefs[0] || '';
        const canonicalSelfChatRef = ownChatJid() || chatRef || canonicalSelfPhoneRef;
        const preferredRef = isSelfChat
          ? canonicalSelfPhoneRef || canonicalSelfChatRef || phoneRef || chatRef || participantRef
          : phoneRef || chatRef || participantRef;
        const senderRef = preferredRef;
        if (!senderRef) continue;
        rememberInboundMessage(msg);
        const relatedRefs = rememberMessageRefs(
          msg.key.id,
          isSelfChat
            ? [...selfRefs, chatRef, participantRef, phoneRef, senderRef]
            : [chatRef, participantRef, phoneRef, senderRef]
        );
        mergeKnownChats(
          relatedRefs,
          msg.pushName || senderRef,
          isSelfChat ? canonicalSelfChatRef : preferredRef,
          isSelfChat ? canonicalSelfPhoneRef || phoneRef : phoneRef
        );

        logger.info(
          {
            messageId: msg.key.id || null,
            fromMe: Boolean(msg.key.fromMe),
            isSelfChat,
            isLikelySelfChatFromLid,
            selfRefs,
            chatRef,
            participantRef,
            phoneRef: phoneRef || null,
            preferredRef,
            senderRef,
            pushName: msg.pushName || null,
            text,
          },
          'WhatsApp inbound message received'
        );

        const payload = {
          sender_ref: senderRef,
          text,
          metadata: {
            message_guid: msg.key.id || null,
            reply_to_ref: extractReplyToRef(msg),
            chat_ref: chatRef || null,
            participant_ref: participantRef || null,
            phone_ref: isSelfChat ? canonicalSelfPhoneRef || null : phoneRef || null,
            transport: 'whatsapp',
            self_chat_whole_thread: isSelfChat,
          },
          attachments: inboundAttachments.length ? inboundAttachments : null,
        };

        logger.info(
          {
            seluInboundUrl: SELU_INBOUND_URL,
            senderRef: payload.sender_ref,
            chatRef: payload.metadata.chat_ref,
            participantRef: payload.metadata.participant_ref,
            messageGuid: payload.metadata.message_guid,
          },
          'Forwarding WhatsApp inbound message to Selu'
        );

        await postInbound(payload);
      }
    } catch (err) {
      logger.error({ err }, 'Failed to process inbound WhatsApp message');
    }
  });
}

const app = express();
app.use(express.json({ limit: '1mb' }));

app.get('/health', (_req, res) => {
  res.json({ ok: true, connection_state: connectionState, jid: currentJid });
});

app.get('/session/status', (_req, res) => {
  res.json({
    ok: true,
    connection_state: connectionState,
    requires_qr: connectionState === 'qr_required',
    qr_available: Boolean(latestQrDataUrl),
    jid: currentJid,
    last_error: lastError,
  });
});

app.get('/session/qr', (_req, res) => {
  if (!latestQrDataUrl) {
    return res.status(404).json({ ok: false, message: 'QR not available' });
  }
  return res.json({ ok: true, qr_data_url: latestQrDataUrl });
});

app.get('/chats', (req, res) => {
  ensureOwnChatEntries();
  const q = (req.query.q || '').toString().trim().toLowerCase();
  let chats = Array.from(knownChats.values())
    .sort((a, b) => (b.updated_at || 0) - (a.updated_at || 0))
    .map((c) => ({ sender_ref: c.sender_ref, chat_ref: c.chat_ref, label: c.label }));

  if (q) {
    chats = chats.filter((c) => {
      const sender = (c.sender_ref || '').toLowerCase();
      const chat = (c.chat_ref || '').toLowerCase();
      const label = (c.label || '').toLowerCase();
      return sender.includes(q) || chat.includes(q) || label.includes(q);
    });
  }

  res.json({
    ok: true,
    chats,
    count: chats.length,
    connection_state: connectionState,
  });
});

app.post('/session/reconnect', async (_req, res) => {
  try {
    await startSocket();
    res.json({ ok: true, message: 'Reconnect started' });
  } catch (err) {
    logger.error({ err }, 'Reconnect failed');
    res.status(500).json({ ok: false, message: 'Reconnect failed' });
  }
});

app.post('/webhooks/selu/outbound', async (req, res) => {
  try {
    if (BRIDGE_EXPECT_AUTH) {
      const provided = (req.headers.authorization || '').trim();
      if (provided !== BRIDGE_EXPECT_AUTH) {
        return res.status(401).json({ ok: false, message: 'Unauthorized' });
      }
    }

    if (!sock) {
      return res.status(503).json({ ok: false, message: 'WhatsApp is not connected yet' });
    }

    const envelope = req.body || {};
    const rawRecipient = (envelope.recipient_ref || '').toString();
    const known = knownChats.get(rawRecipient) || null;
    const text = (envelope.text || '').toString();
    const attachments = Array.isArray(envelope.attachments) ? envelope.attachments : [];
    const replyToMessageRef = (envelope.reply_to_message_ref || '').toString().trim();
    const quoted = lookupQuotedMessage(replyToMessageRef);
    const recipients = quotedRecipientCandidates(quoted, rawRecipient);
    const isSelfRecipient = isOwnRecipient(rawRecipient);

    if (!recipients.length || (!text.trim() && attachments.length === 0)) {
      return res.status(400).json({ ok: false, message: 'recipient_ref and text or attachments are required' });
    }

    logger.info(
      {
        rawRecipient,
        normalizedRecipient: normalizeRecipient(rawRecipient),
        recipientCandidates: recipients,
        selfRecipientCandidates: isSelfRecipient ? selfRecipientCandidates() : [],
        replyToMessageRef: replyToMessageRef || null,
        hasQuotedMessage: Boolean(quoted),
        knownChat: known,
        textLength: text.trim().length,
        attachmentsCount: attachments.length,
        isSelfRecipient,
      },
      'WhatsApp outbound request received'
    );

    let sent = null;
    let lastErr = null;
    for (const recipient of recipients) {
      try {
        const startedAt = Date.now();
        // Quoted replies must go back to the same chat JID as the original
        // message. If we remap to a phone/chat fallback, send plain text
        // instead of attaching a mismatched quoted payload.
        const canUseQuoted =
          !isSelfRecipient &&
          Boolean(quoted) &&
          Boolean(quoted.key) &&
          String(quoted.key.remoteJid || '').trim() === recipient;
        if (text.trim()) {
          sent = await sock.sendMessage(
            recipient,
            { text },
            canUseQuoted ? { quoted } : undefined
          );
        }

        for (const attachment of attachments) {
          const mimeType = (attachment && attachment.mime_type ? attachment.mime_type : '').toString();
          const fileName = (attachment && attachment.filename ? attachment.filename : 'attachment.bin').toString();
          const downloadUrl = (attachment && attachment.download_url ? attachment.download_url : '').toString();
          const dataBase64 = (attachment && attachment.data_base64 ? attachment.data_base64 : '').toString();
          const sendOptions = canUseQuoted ? { quoted } : undefined;

          if (mimeType.startsWith('image/')) {
            if (dataBase64) {
              sent = await sock.sendMessage(
                recipient,
                {
                  image: Buffer.from(dataBase64, 'base64'),
                  mimetype: mimeType || 'image/jpeg',
                },
                sendOptions
              );
            } else if (downloadUrl) {
              sent = await sock.sendMessage(
                recipient,
                {
                  image: { url: downloadUrl },
                  mimetype: mimeType || 'image/jpeg',
                },
                sendOptions
              );
            }
            continue;
          }

          // Keep document delivery for PDFs and other non-image files.
          if (dataBase64) {
            sent = await sock.sendMessage(
              recipient,
              {
                document: Buffer.from(dataBase64, 'base64'),
                mimetype: mimeType || 'application/octet-stream',
                fileName,
              },
              sendOptions
            );
          } else if (downloadUrl) {
            sent = await sock.sendMessage(
              recipient,
              {
                document: { url: downloadUrl },
                mimetype: mimeType || 'application/octet-stream',
                fileName,
              },
              sendOptions
            );
          }
        }
        const sendLatencyMs = Date.now() - startedAt;
        const sentMessageId = sent && sent.key ? sent.key.id || null : null;
        rememberOutboundMessage(sentMessageId || '', {
          recipient,
          rawRecipient,
          remoteJid: sent && sent.key ? sent.key.remoteJid || null : null,
          replyToMessageRef: replyToMessageRef || null,
          usedQuotedMessage: canUseQuoted,
          send_latency_ms: sendLatencyMs,
        });
        logger.info(
          {
            recipient,
            rawRecipient,
            replyToMessageRef: replyToMessageRef || null,
            usedQuotedMessage: canUseQuoted,
            sendLatencyMs,
            messageId: sent && sent.key ? sent.key.id || null : null,
            remoteJid: sent && sent.key ? sent.key.remoteJid || null : null,
          },
          'WhatsApp outbound sent'
        );
        break;
      } catch (err) {
        lastErr = err;
        logger.warn({ err, recipient }, 'WhatsApp outbound send failed for recipient');
      }
    }

    if (!sent) {
      throw lastErr || new Error('All recipient candidates failed');
    }

    res.json({
      ok: true,
      message_ref: sent && sent.key ? sent.key.id || null : null,
    });
  } catch (err) {
    logger.error({ err }, 'Failed to send WhatsApp outbound message');
    res.status(500).json({ ok: false, message: 'Failed to send message' });
  }
});

app.listen(PORT, async () => {
  logger.info({ port: PORT }, 'Selu WhatsApp bridge listening');
  try {
    await startSocket();
  } catch (err) {
    logger.error({ err }, 'Failed to initialize WhatsApp socket');
    connectionState = 'error';
    lastError = 'Failed to initialize WhatsApp socket';
  }
});
