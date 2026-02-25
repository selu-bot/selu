"""
PAP Capability Container: PIM API (Email + Calendar)

Implements the pap.capability.v1.Capability gRPC service.
Provides IMAP email reading/searching, SMTP email sending,
and CalDAV calendar access.

Credentials are injected per-invocation by the orchestrator via config_json.
"""

import email
import email.header
import email.utils
import html
import imaplib
import json
import logging
import os
import re
import signal
import smtplib
import ssl
import sys
from concurrent import futures
from datetime import datetime, timedelta, timezone
from email.mime.text import MIMEText

import grpc

import capability_pb2
import capability_pb2_grpc

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(message)s",
)
log = logging.getLogger("pim-api")

GRPC_PORT = 50051

# ── Helpers ────────────────────────────────────────────────────────────────────


def _decode_header(raw: str | None) -> str:
    """Decode RFC 2047 encoded header value."""
    if not raw:
        return ""
    parts = email.header.decode_header(raw)
    decoded = []
    for data, charset in parts:
        if isinstance(data, bytes):
            decoded.append(data.decode(charset or "utf-8", errors="replace"))
        else:
            decoded.append(data)
    return " ".join(decoded)


def _get_text_body(msg: email.message.Message, max_len: int = 2000) -> str:
    """Extract plain-text body from an email message."""
    if msg.is_multipart():
        for part in msg.walk():
            ct = part.get_content_type()
            if ct == "text/plain":
                payload = part.get_payload(decode=True)
                if payload:
                    charset = part.get_content_charset() or "utf-8"
                    text = payload.decode(charset, errors="replace")
                    return text[:max_len]
        # Fallback: try text/html and strip tags
        for part in msg.walk():
            ct = part.get_content_type()
            if ct == "text/html":
                payload = part.get_payload(decode=True)
                if payload:
                    charset = part.get_content_charset() or "utf-8"
                    raw_html = payload.decode(charset, errors="replace")
                    text = re.sub(r"<[^>]+>", "", raw_html)
                    text = html.unescape(text).strip()
                    return text[:max_len]
    else:
        payload = msg.get_payload(decode=True)
        if payload:
            charset = msg.get_content_charset() or "utf-8"
            text = payload.decode(charset, errors="replace")
            return text[:max_len]
    return ""


def _connect_imap(config: dict) -> imaplib.IMAP4_SSL:
    """Connect and authenticate to the IMAP server."""
    host = config["imap_host"]
    username = config["imap_username"]
    password = config["imap_password"]
    port = int(config.get("imap_port", 993))

    ctx = ssl.create_default_context()
    conn = imaplib.IMAP4_SSL(host, port, ssl_context=ctx)
    conn.login(username, password)
    return conn


def _connect_smtp(config: dict) -> smtplib.SMTP_SSL:
    """Connect and authenticate to the SMTP server."""
    host = config["smtp_host"]
    username = config["smtp_username"]
    password = config["smtp_password"]
    port = int(config.get("smtp_port", 465))

    ctx = ssl.create_default_context()
    conn = smtplib.SMTP_SSL(host, port, context=ctx)
    conn.login(username, password)
    return conn


# ── Email Tools ────────────────────────────────────────────────────────────────


def list_mailboxes(config: dict, args: dict) -> dict:
    """List all mailboxes (folders)."""
    conn = _connect_imap(config)
    try:
        status, data = conn.list()
        if status != "OK":
            raise RuntimeError(f"IMAP LIST failed: {status}")

        mailboxes = []
        for item in data:
            if isinstance(item, bytes):
                # Format: (\Flags) "delimiter" "name"
                decoded = item.decode("utf-8", errors="replace")
                # Extract the mailbox name (last quoted or unquoted segment)
                match = re.search(r'"([^"]*)"$|(\S+)$', decoded)
                if match:
                    name = match.group(1) or match.group(2)
                    mailboxes.append({"name": name})

        return {"mailboxes": mailboxes}
    finally:
        conn.logout()


def fetch_emails(config: dict, args: dict) -> dict:
    """Fetch recent emails from a mailbox."""
    mailbox = args.get("mailbox", "INBOX")
    limit = min(args.get("limit", 10), 50)

    conn = _connect_imap(config)
    try:
        status, _ = conn.select(mailbox, readonly=True)
        if status != "OK":
            raise RuntimeError(f"Cannot select mailbox '{mailbox}'")

        # Get the latest N message UIDs
        status, data = conn.uid("search", None, "ALL")
        if status != "OK":
            return {"emails": [], "mailbox": mailbox}

        uids = data[0].split() if data[0] else []
        uids = uids[-limit:]  # most recent
        uids.reverse()  # newest first

        emails = []
        for uid in uids:
            status, msg_data = conn.uid("fetch", uid, "(BODY.PEEK[HEADER] INTERNALDATE)")
            if status != "OK" or not msg_data or not msg_data[0]:
                continue

            raw_header = msg_data[0][1] if isinstance(msg_data[0], tuple) else msg_data[0]
            if isinstance(raw_header, bytes):
                msg = email.message_from_bytes(raw_header)
            else:
                continue

            date_str = email.utils.parsedate_to_datetime(
                msg.get("Date", "")
            ).isoformat() if msg.get("Date") else ""

            emails.append({
                "uid": int(uid),
                "from": _decode_header(msg.get("From")),
                "to": _decode_header(msg.get("To")),
                "subject": _decode_header(msg.get("Subject")),
                "date": date_str,
            })

        return {"emails": emails, "mailbox": mailbox, "count": len(emails)}
    finally:
        conn.logout()


def search_emails(config: dict, args: dict) -> dict:
    """Search emails by criteria."""
    mailbox = args.get("mailbox", "INBOX")
    limit = min(args.get("limit", 10), 50)

    conn = _connect_imap(config)
    try:
        status, _ = conn.select(mailbox, readonly=True)
        if status != "OK":
            raise RuntimeError(f"Cannot select mailbox '{mailbox}'")

        # Build IMAP search criteria
        criteria = []
        if args.get("from"):
            criteria.append(f'FROM "{args["from"]}"')
        if args.get("subject"):
            criteria.append(f'SUBJECT "{args["subject"]}"')
        if args.get("since"):
            # IMAP date format: DD-Mon-YYYY
            dt = datetime.strptime(args["since"], "%Y-%m-%d")
            criteria.append(f'SINCE {dt.strftime("%d-%b-%Y")}')
        if args.get("before"):
            dt = datetime.strptime(args["before"], "%Y-%m-%d")
            criteria.append(f'BEFORE {dt.strftime("%d-%b-%Y")}')
        if args.get("body"):
            criteria.append(f'BODY "{args["body"]}"')

        if not criteria:
            criteria.append("ALL")

        search_str = " ".join(criteria)
        status, data = conn.uid("search", None, search_str)
        if status != "OK":
            return {"emails": [], "mailbox": mailbox, "query": search_str}

        uids = data[0].split() if data[0] else []
        uids = uids[-limit:]
        uids.reverse()

        emails = []
        for uid in uids:
            status, msg_data = conn.uid("fetch", uid, "(BODY.PEEK[HEADER])")
            if status != "OK" or not msg_data or not msg_data[0]:
                continue

            raw_header = msg_data[0][1] if isinstance(msg_data[0], tuple) else msg_data[0]
            if isinstance(raw_header, bytes):
                msg = email.message_from_bytes(raw_header)
            else:
                continue

            date_str = ""
            if msg.get("Date"):
                try:
                    date_str = email.utils.parsedate_to_datetime(msg["Date"]).isoformat()
                except Exception:
                    date_str = msg["Date"]

            emails.append({
                "uid": int(uid),
                "from": _decode_header(msg.get("From")),
                "subject": _decode_header(msg.get("Subject")),
                "date": date_str,
            })

        return {
            "emails": emails,
            "mailbox": mailbox,
            "query": search_str,
            "count": len(emails),
        }
    finally:
        conn.logout()


def read_email(config: dict, args: dict) -> dict:
    """Read the full body of a specific email by UID."""
    mailbox = args.get("mailbox", "INBOX")
    uid = args.get("uid")
    if uid is None:
        raise ValueError("'uid' is required")

    conn = _connect_imap(config)
    try:
        status, _ = conn.select(mailbox, readonly=True)
        if status != "OK":
            raise RuntimeError(f"Cannot select mailbox '{mailbox}'")

        uid_str = str(uid).encode()
        status, msg_data = conn.uid("fetch", uid_str, "(RFC822)")
        if status != "OK" or not msg_data or not msg_data[0]:
            raise RuntimeError(f"Email UID {uid} not found in {mailbox}")

        raw = msg_data[0][1] if isinstance(msg_data[0], tuple) else msg_data[0]
        if not isinstance(raw, bytes):
            raise RuntimeError("Unexpected IMAP response format")

        msg = email.message_from_bytes(raw)

        date_str = ""
        if msg.get("Date"):
            try:
                date_str = email.utils.parsedate_to_datetime(msg["Date"]).isoformat()
            except Exception:
                date_str = msg["Date"]

        return {
            "uid": uid,
            "from": _decode_header(msg.get("From")),
            "to": _decode_header(msg.get("To")),
            "cc": _decode_header(msg.get("Cc")),
            "subject": _decode_header(msg.get("Subject")),
            "date": date_str,
            "message_id": msg.get("Message-ID", ""),
            "body": _get_text_body(msg, max_len=8000),
        }
    finally:
        conn.logout()


def send_email(config: dict, args: dict) -> dict:
    """Send an email via SMTP."""
    to = args.get("to")
    subject = args.get("subject")
    body = args.get("body")
    cc = args.get("cc", "")
    from_addr = config["smtp_from"]

    if not to or not subject or not body:
        raise ValueError("'to', 'subject', and 'body' are required")

    msg = MIMEText(body, "plain", "utf-8")
    msg["From"] = from_addr
    msg["To"] = to
    msg["Subject"] = subject
    if cc:
        msg["Cc"] = cc

    # Handle reply threading
    reply_to_uid = args.get("reply_to_uid")
    if reply_to_uid is not None:
        try:
            original = read_email(config, {"uid": reply_to_uid})
            if original.get("message_id"):
                msg["In-Reply-To"] = original["message_id"]
                msg["References"] = original["message_id"]
        except Exception as e:
            log.warning("Could not fetch original message for reply: %s", e)

    # Collect all recipients
    recipients = [addr.strip() for addr in to.split(",")]
    if cc:
        recipients.extend(addr.strip() for addr in cc.split(","))

    conn = _connect_smtp(config)
    try:
        conn.sendmail(from_addr, recipients, msg.as_string())
    finally:
        conn.quit()

    return {
        "status": "sent",
        "to": to,
        "subject": subject,
        "cc": cc or None,
    }


# ── Calendar Tools ─────────────────────────────────────────────────────────────


def list_calendars(config: dict, args: dict) -> dict:
    """List CalDAV calendars."""
    import caldav

    url = config.get("caldav_url")
    username = config.get("caldav_username")
    password = config.get("caldav_password")

    if not url or not username or not password:
        return {"error": "CalDAV credentials not configured. Please set caldav_url, caldav_username, and caldav_password."}

    client = caldav.DAVClient(url=url, username=username, password=password)
    principal = client.principal()
    calendars = principal.calendars()

    result = []
    for cal in calendars:
        props = cal.get_properties([caldav.dav.DisplayName()])
        display_name = str(props.get(caldav.dav.DisplayName.tag, cal.name or "Unnamed"))
        result.append({
            "name": display_name,
            "url": str(cal.url),
        })

    return {"calendars": result}


def list_events(config: dict, args: dict) -> dict:
    """List calendar events in a date range."""
    import caldav
    from icalendar import Calendar

    url = config.get("caldav_url")
    username = config.get("caldav_username")
    password = config.get("caldav_password")

    if not url or not username or not password:
        return {"error": "CalDAV credentials not configured. Please set caldav_url, caldav_username, and caldav_password."}

    # Parse date range
    now = datetime.now(timezone.utc)
    start_str = args.get("start_date")
    end_str = args.get("end_date")

    if start_str:
        start = datetime.strptime(start_str, "%Y-%m-%d").replace(tzinfo=timezone.utc)
    else:
        start = now.replace(hour=0, minute=0, second=0, microsecond=0)

    if end_str:
        end = datetime.strptime(end_str, "%Y-%m-%d").replace(tzinfo=timezone.utc)
    else:
        end = start + timedelta(days=7)

    target_calendar = args.get("calendar")

    client = caldav.DAVClient(url=url, username=username, password=password)
    principal = client.principal()
    calendars = principal.calendars()

    events = []
    for cal in calendars:
        props = cal.get_properties([caldav.dav.DisplayName()])
        cal_name = str(props.get(caldav.dav.DisplayName.tag, cal.name or "Unnamed"))

        if target_calendar and cal_name.lower() != target_calendar.lower():
            continue

        try:
            results = cal.date_search(start=start, end=end, expand=True)
        except Exception as e:
            log.warning("date_search failed for calendar %s: %s", cal_name, e)
            continue

        for event_obj in results:
            try:
                ical = Calendar.from_ical(event_obj.data)
                for component in ical.walk():
                    if component.name == "VEVENT":
                        dtstart = component.get("DTSTART")
                        dtend = component.get("DTEND")

                        start_dt = dtstart.dt if dtstart else None
                        end_dt = dtend.dt if dtend else None

                        # Convert date to datetime for consistent formatting
                        if start_dt and not isinstance(start_dt, datetime):
                            start_dt = datetime.combine(start_dt, datetime.min.time(), tzinfo=timezone.utc)
                        if end_dt and not isinstance(end_dt, datetime):
                            end_dt = datetime.combine(end_dt, datetime.min.time(), tzinfo=timezone.utc)

                        events.append({
                            "calendar": cal_name,
                            "summary": str(component.get("SUMMARY", "Untitled")),
                            "start": start_dt.isoformat() if start_dt else None,
                            "end": end_dt.isoformat() if end_dt else None,
                            "location": str(component.get("LOCATION", "")) or None,
                            "description": str(component.get("DESCRIPTION", ""))[:500] or None,
                        })
            except Exception as e:
                log.warning("Failed to parse event: %s", e)

    # Sort by start time
    events.sort(key=lambda e: e.get("start") or "")

    return {
        "events": events,
        "count": len(events),
        "range": {"start": start.strftime("%Y-%m-%d"), "end": end.strftime("%Y-%m-%d")},
    }


# ── Tool dispatch ──────────────────────────────────────────────────────────────

TOOLS = {
    "list_mailboxes": list_mailboxes,
    "fetch_emails": fetch_emails,
    "search_emails": search_emails,
    "read_email": read_email,
    "send_email": send_email,
    "list_calendars": list_calendars,
    "list_events": list_events,
}


class CapabilityServicer(capability_pb2_grpc.CapabilityServicer):
    """Implements the pap.capability.v1.Capability gRPC service."""

    def Healthcheck(self, request, context):
        return capability_pb2.HealthResponse(ready=True, message="pim-api ready")

    def Invoke(self, request, context):
        tool = request.tool_name
        log.info("Invoke tool=%s", tool)

        handler = TOOLS.get(tool)
        if handler is None:
            return capability_pb2.InvokeResponse(error=f"Unknown tool: '{tool}'")

        try:
            args = json.loads(request.args_json) if request.args_json else {}
            config = json.loads(request.config_json) if request.config_json else {}

            result = handler(config, args)

            return capability_pb2.InvokeResponse(
                result_json=json.dumps(result).encode("utf-8")
            )

        except Exception as e:
            log.exception("Error during %s invocation", tool)
            return capability_pb2.InvokeResponse(error=str(e))

    def StreamInvoke(self, request, context):
        resp = self.Invoke(request, context)
        if resp.error:
            yield capability_pb2.InvokeChunk(error=resp.error, done=True)
        else:
            yield capability_pb2.InvokeChunk(data=resp.result_json, done=True)


def serve():
    server = grpc.server(futures.ThreadPoolExecutor(max_workers=4))
    capability_pb2_grpc.add_CapabilityServicer_to_server(
        CapabilityServicer(), server
    )
    server.add_insecure_port(f"0.0.0.0:{GRPC_PORT}")
    server.start()
    log.info("PIM API capability listening on port %d", GRPC_PORT)

    def _shutdown(signum, frame):
        log.info("Shutting down...")
        server.stop(grace=5)
        sys.exit(0)

    signal.signal(signal.SIGTERM, _shutdown)
    server.wait_for_termination()


if __name__ == "__main__":
    serve()
