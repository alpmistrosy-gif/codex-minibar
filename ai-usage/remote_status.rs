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

pub fn enabled() -> bool {
    true
}

pub fn handles_provider(provider: ProviderKind) -> bool {
    matches!(provider, ProviderKind::Codex | ProviderKind::Claude)
}

pub fn endpoint_label() -> &'static str {
    "Home Linux · Tailscale · FRESH"
}

pub fn read_provider(provider: ProviderKind) -> Result<RateLimits> {
    if !handles_provider(provider) {
        bail!("provider is not served by Home Linux AI usage");
    }
    let payload = fetch_status()?;
    parse_provider(&payload, provider)
}

fn fetch_status() -> Result<Value> {
    let address = SocketAddr::new(IpAddr::V4(HOST), PORT);
    let mut stream =
        TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).context("connect Home Linux AI usage")?;
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
    if root.get("schema").and_then(Value::as_u64) != Some(1) {
        bail!("unsupported Home Linux AI usage schema");
    }
    if root.get("freshness").and_then(Value::as_str) != Some("FRESH") {
        bail!("Home Linux AI usage snapshot is not fresh");
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
        plan_type: value.get("plan").and_then(Value::as_str).map(str::to_owned),
        account_name: Some(format!("Home Linux · FRESH · {trust}")),
        limit_name: Some(source.to_owned()),
        credits: parse_credits(value.get("credits")),
        usage: Default::default(),
        ..RateLimits::default()
    })
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
        balance: value
            .get("balance")
            .and_then(|v| match v {
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

    #[test]
    fn maps_codex_and_claude_windows() {
        let payload = json!({
            "schema": 1,
            "generated_at": "2026-09-06T15:11:35Z",
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
