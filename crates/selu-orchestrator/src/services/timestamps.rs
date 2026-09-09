use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, SecondsFormat, TimeZone, Utc};

/// Parse a transport or legacy database timestamp as an absolute UTC instant.
///
/// RFC 3339 values retain their encoded offset. Legacy offset-less SQLite
/// values are accepted only at this compatibility boundary and are UTC.
pub fn parse_utc(value: &str) -> Result<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Ok(parsed.with_timezone(&Utc));
    }

    for format in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(parsed) = NaiveDateTime::parse_from_str(value, format) {
            return Ok(Utc.from_utc_datetime(&parsed));
        }
    }

    anyhow::bail!("invalid UTC timestamp '{value}'")
}

/// Canonical API representation: RFC 3339 UTC with millisecond precision.
pub fn canonical_utc(value: &str) -> Result<String> {
    Ok(parse_utc(value)
        .with_context(|| format!("failed to canonicalize timestamp '{value}'"))?
        .to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// Stable SQLite representation for schedule comparison and indexing.
pub fn to_db_utc(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_offsets_and_legacy_values_to_the_same_contract() {
        assert_eq!(
            canonical_utc("2026-09-09T10:15:30.125+02:00").unwrap(),
            "2026-09-09T08:15:30.125Z"
        );
        assert_eq!(
            canonical_utc("2026-09-09 08:15:30").unwrap(),
            "2026-09-09T08:15:30.000Z"
        );
        assert_eq!(
            canonical_utc("2026-09-09T08:15:30.125").unwrap(),
            "2026-09-09T08:15:30.125Z"
        );
    }
}
