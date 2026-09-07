use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::{
    limits::{Credits, LimitWindow, RateLimits},
    settings::ProviderKind,
};

const HOST: Ipv4Addr = Ipv4Addr::new(100, 102, 168, 71);
const PORT: u16 = 8767;
const PATH: &str = "/ai-usage.json";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

#[cfg(not(test))]
pub fn enabled() -> bool {
    true
}

#[cfg(test)]
pub fn enabled() -> bool {
    false
}

pub fn handles_provider(provider: ProviderKind) -> bool {
    matches!(provider, ProviderKind::Codex | ProviderKind::Claude)
}

pub fn endpoint_label() -> &'static str {
    "Home Linux · Tailscale · HTTP"
}

pub fn read_provider(provider: ProviderKind) -> Result<RateLimits> {
    if !handles_provider(provider) {
        bail!("provider is not served by Home Linux AI usage");
    }
    let payload = fetch_status()?;
    parse_provider(&payload, provider)
}

pub fn unknown_limits() -> RateLimits {
    RateLimits {
        account_name: Some("Home Linux · UNKNOWN".to_owned()),
        ..RateLimits::default()
    }
}

pub fn expire_limits(limits: &mut RateLimits, now: DateTime<Utc>) -> bool {
    if limits.remote_expires_at.is_some_and(|expiry| now >= expiry) {
        *limits = unknown_limits();
        return true;
    }
    false
}

fn fetch_status() -> Result<Value> {
    let address = SocketAddr::new(IpAddr::V4(HOST), PORT);
    let mut stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
        .context("connect Home Linux AI usage")?;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .context("set AI usage read timeout")?;
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .context("set AI usage write timeout")?;

    write!(
        stream,
        "GET {PATH} HTTP/1.1\r\nHost: {HOST}:{PORT}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    )
    .context("write AI usage request")?;
    stream.flush().context("flush AI usage request")?;

    let mut bytes = Vec::new();
    (&mut stream)
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("read AI usage response")?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        bail!("Home Linux AI usage response is too large");
    }

    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("invalid HTTP response from Home Linux AI usage")?;
    let header = std::str::from_utf8(&bytes[..split]).context("invalid HTTP headers")?;
    let status = header.lines().next().unwrap_or_default();
    if !(status.starts_with("HTTP/1.1 200 ") || status.starts_with("HTTP/1.0 200 ")) {
        bail!("Home Linux AI usage returned {status}");
    }

    let body = &bytes[split + 4..];
    let payload: Value = serde_json::from_slice(body).context("parse Home Linux AI usage JSON")?;
    Ok(payload)
}

fn parse_provider(root: &Value, provider: ProviderKind) -> Result<RateLimits> {
    parse_provider_at(root, provider, Utc::now())
}

fn parse_provider_at(
    root: &Value,
    provider: ProviderKind,
    now: DateTime<Utc>,
) -> Result<RateLimits> {
    if root.get("schema").and_then(Value::as_u64) != Some(1) {
        bail!("unsupported Home Linux AI usage schema");
    }
    if root.get("freshness").and_then(Value::as_str) != Some("FRESH") {
        bail!("Home Linux AI usage snapshot is not fresh");
    }

    let generated = parse_time(root.get("generated_at").and_then(Value::as_str))?;
    let expires = parse_time(root.get("expires_at").and_then(Value::as_str))?;
    let ttl = root
        .get("ttl_seconds")
        .and_then(Value::as_i64)
        .filter(|ttl| *ttl > 0 && *ttl <= 600)
        .context("invalid snapshot TTL")?;
    if generated > now || now >= expires || expires > generated + chrono::Duration::seconds(ttl) {
        bail!("Home Linux snapshot is expired or has invalid timestamps");
    }

    let provider_id = match provider {
        ProviderKind::Codex => "codex",
        ProviderKind::Claude => "claude",
        _ => unreachable!(),
    };
    let value = root
        .pointer(&format!("/providers/{provider_id}"))
        .and_then(Value::as_object)
        .context("provider missing from Home Linux AI usage snapshot")?;

    if value.get("freshness").and_then(Value::as_str) != Some("FRESH") {
        bail!("{provider_id} Home Linux snapshot is not fresh");
    }

    let sampled_at = parse_time(
        value
            .get("as_of")
            .or_else(|| root.get("generated_at"))
            .and_then(Value::as_str),
    )?;
    if sampled_at > now || now >= sampled_at + chrono::Duration::seconds(ttl) {
        bail!("Home Linux provider sample has expired");
    }
    let windows = value
        .get("windows")
        .and_then(Value::as_object)
        .context("provider windows missing from Home Linux snapshot")?;

    let (primary_key, secondary_key, primary_duration, secondary_duration) = match provider {
        ProviderKind::Codex => ("five_hour", "weekly", Some(300), Some(10_080)),
        ProviderKind::Claude => ("five_hour", "seven_day", Some(300), Some(10_080)),
        _ => unreachable!(),
    };

    let primary = parse_window(windows.get(primary_key), primary_duration)?;
    let secondary = parse_window(windows.get(secondary_key), secondary_duration)?;

    let source = value
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("Home Linux normalized usage");
    let trust = value
        .get("trust")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    Ok(RateLimits {
        primary,
        secondary,
        sampled_at,
        remote_expires_at: Some(expires.min(sampled_at + chrono::Duration::seconds(ttl))),
        plan_type: value.get("plan").and_then(Value::as_str).map(str::to_owned),
        account_name: Some(remote_status_caption(provider, source, trust)),
        limit_name: Some(source.to_owned()),
        credits: parse_credits(value.get("credits")),
        usage: Default::default(),
        ..RateLimits::default()
    })
}

fn remote_status_caption(provider: ProviderKind, source: &str, trust: &str) -> String {
    let source_label = match provider {
        ProviderKind::Codex if source.contains("app-server") => "app-server",
        ProviderKind::Claude if source.to_ascii_lowercase().contains("oauth") => "OAuth usage",
        _ => "normalized",
    };
    let trust_label = match trust {
        "supported_local_app_server" => "supported",
        "undocumented_fallback" => "fallback",
        "native_session" => "native",
        _ => "unverified",
    };
    format!("Home Linux · FRESH · {source_label} · {trust_label}")
}

fn parse_window(value: Option<&Value>, fallback_duration: Option<u32>) -> Result<LimitWindow> {
    let Some(value) = value else {
        return Ok(LimitWindow::default());
    };
    if value.get("state").and_then(Value::as_str) != Some("FRESH") {
        return Ok(LimitWindow::default());
    }

    let used = value
        .get("used_percent")
        .and_then(Value::as_f64)
        .context("fresh quota window has no used_percent")?;
    if !used.is_finite() {
        bail!("quota used_percent is not finite");
    }

    let resets_at = value
        .get("reset_at")
        .and_then(Value::as_str)
        .map(|raw| parse_time(Some(raw)))
        .transpose()?;

    let duration_minutes = value
        .get("window_minutes")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .or(fallback_duration);

    Ok(LimitWindow {
        used_percent: Some(used.round().clamp(0.0, 100.0) as u8),
        resets_at,
        duration_minutes,
    })
}

fn parse_credits(value: Option<&Value>) -> Credits {
    let Some(value) = value else {
        return Credits::default();
    };
    Credits {
        has_credits: value
            .get("has_credits")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        unlimited: value
            .get("unlimited")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        balance: value.get("balance").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }),
    }
}

fn parse_time(value: Option<&str>) -> Result<DateTime<Utc>> {
    let value = value.context("Home Linux snapshot timestamp missing")?;
    Ok(DateTime::parse_from_rfc3339(value)
        .context("invalid Home Linux snapshot timestamp")?
        .with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_provider(root: &Value, provider: ProviderKind) -> Result<RateLimits> {
        parse_provider_at(root, provider, parse_time(Some("2026-09-06T15:12:00Z"))?)
    }

    #[test]
    fn expiry_clears_both_windows_at_deadline() {
        let now = parse_time(Some("2026-09-06T15:12:00Z")).unwrap();
        let mut limits = RateLimits {
            primary: LimitWindow {
                used_percent: Some(42),
                ..LimitWindow::default()
            },
            secondary: LimitWindow {
                used_percent: Some(63),
                ..LimitWindow::default()
            },
            remote_expires_at: Some(now),
            ..RateLimits::default()
        };
        assert!(!expire_limits(&mut limits, now - chrono::Duration::seconds(1)));
        assert!(expire_limits(&mut limits, now));
        assert_eq!(limits, unknown_limits());
        assert!(!expire_limits(&mut limits, now));
    }

    #[test]
    fn maps_codex_and_claude_windows() {
        let payload = json!({
            "schema": 1,
            "generated_at": "2026-09-06T15:11:35Z",
            "expires_at": "2026-09-06T15:21:35Z",
            "ttl_seconds": 600,
            "freshness": "FRESH",
            "providers": {
                "codex": {
                    "freshness": "FRESH",
                    "as_of": "2026-09-06T15:09:56Z",
                    "source": "codex app-server account/rateLimits/read",
                    "trust": "supported_local_app_server",
                    "plan": "plus",
                    "windows": {
                        "five_hour": {"state":"FRESH","used_percent":100,"remaining_percent":0,"window_minutes":300,"reset_at":"2026-09-06T17:34:29Z"},
                        "weekly": {"state":"FRESH","used_percent":63,"remaining_percent":37,"window_minutes":10080,"reset_at":"2026-09-12T14:59:22Z"}
                    }
                },
                "claude": {
                    "freshness": "FRESH",
                    "as_of": "2026-09-06T15:11:36Z",
                    "source": "anthropic_oauth_usage_via_claude_code_session",
                    "trust": "undocumented_fallback",
                    "plan": "pro",
                    "windows": {
                        "five_hour": {"state":"FRESH","used_percent":18.0,"remaining_percent":82.0,"reset_at":"2026-09-06T16:40:00Z"},
                        "seven_day": {"state":"FRESH","used_percent":100.0,"remaining_percent":0.0,"reset_at":"2026-09-06T19:00:00Z"}
                    }
                }
            }
        });

        let codex = parse_provider(&payload, ProviderKind::Codex).unwrap();
        assert_eq!(codex.primary.remaining_percent(), Some(0));
        assert_eq!(codex.secondary.remaining_percent(), Some(37));
        assert_eq!(codex.plan_type.as_deref(), Some("plus"));

        let claude = parse_provider(&payload, ProviderKind::Claude).unwrap();
        assert_eq!(claude.primary.remaining_percent(), Some(82));
        assert_eq!(claude.secondary.remaining_percent(), Some(0));
        assert_eq!(claude.plan_type.as_deref(), Some("pro"));
    }

    #[test]
    fn rejects_expired_document_even_when_marked_fresh() {
        let payload = json!({
            "schema": 1,
            "freshness": "FRESH",
            "generated_at": "2026-09-06T15:00:00Z",
            "expires_at": "2026-09-06T15:10:00Z",
            "ttl_seconds": 600
        });
        let error = parse_provider(&payload, ProviderKind::Codex).unwrap_err();
        assert!(error.to_string().contains("expired"));
    }

    #[test]
    fn rejects_stale_document() {
        let payload = json!({
            "schema": 1,
            "freshness": "STALE",
            "providers": {}
        });
        assert!(parse_provider(&payload, ProviderKind::Codex).is_err());
    }

    #[test]
    fn unknown_window_does_not_expose_old_percentage() {
        let payload = json!({
            "schema": 1,
            "generated_at": "2026-09-06T15:11:35Z",
            "expires_at": "2026-09-06T15:21:35Z",
            "ttl_seconds": 600,
            "freshness": "FRESH",
            "providers": {
                "claude": {
                    "freshness": "FRESH",
                    "as_of": "2026-09-06T15:11:36Z",
                    "plan": "pro",
                    "windows": {
                        "five_hour": {"state":"UNKNOWN"},
                        "seven_day": {"state":"FRESH","used_percent":50.0,"reset_at":"2026-09-06T19:00:00Z"}
                    }
                }
            }
        });
        let claude = parse_provider(&payload, ProviderKind::Claude).unwrap();
        assert!(claude.primary.used_percent.is_none());
        assert_eq!(claude.secondary.remaining_percent(), Some(50));
    }
}
