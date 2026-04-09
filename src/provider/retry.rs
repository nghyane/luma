use crate::event::Event;
use anyhow::{Result, bail};
use std::future::Future;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MAX_RETRIES: u8 = 4;
const MAX_RETRY_DELAY_SECS: u64 = 30;
const OPENAI_RESET_HEADERS: &[&str] = &["x-ratelimit-reset-requests", "x-ratelimit-reset-tokens"];

/// Format provider HTTP errors with clearer guidance for TUI.
pub fn format_http_error(provider: &str, status: reqwest::StatusCode, msg: &str) -> String {
    let detail = msg.trim();
    let code = status.as_u16();
    match code {
        429 => {
            if is_hard_quota_error(detail) {
                format!(
                    "{provider} hard quota exceeded (429): {detail}. Quota/billing must recover before retrying; try another model/provider if needed."
                )
            } else {
                format!(
                    "{provider} temporary throttling (429): {detail}. Wait a bit, reduce request frequency, or switch model/provider."
                )
            }
        }
        401 => format!(
            "{provider} auth failed (401): {detail}. Check your API key or run 'luma sync' to refresh credentials."
        ),
        403 => format!(
            "{provider} access denied (403): {detail}. Verify your API key has the required permissions."
        ),
        // Anthropic returns 529 when overloaded
        529 => format!(
            "{provider} overloaded (529): {detail}. The API is temporarily at capacity; retry shortly or switch provider."
        ),
        _ => format!("{status}: {detail}"),
    }
}

/// Format a network/transport error with actionable guidance.
pub fn format_network_error(err: &reqwest::Error) -> String {
    if err.is_connect() {
        return format!(
            "connection failed: {}. Check your internet connection and any proxy/firewall settings.",
            brief_reqwest_cause(err)
        );
    }
    if err.is_timeout() {
        return "request timed out. The provider may be slow or unreachable; try again shortly."
            .to_owned();
    }
    format!("network error: {err}")
}

/// Extract the innermost cause from a reqwest error for a concise message.
fn brief_reqwest_cause(err: &reqwest::Error) -> String {
    let mut source: &dyn std::error::Error = err;
    while let Some(inner) = source.source() {
        source = inner;
    }
    source.to_string()
}

fn is_hard_quota_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("insufficient_quota")
        || lower.contains("quota exceeded")
        || lower.contains("billing")
}

fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    value
        .parse::<u64>()
        .ok()
        .or_else(|| retry_after_http_date_secs(value))
}

fn provider_reset_secs(provider: &str, headers: &reqwest::header::HeaderMap) -> Option<u64> {
    if provider != "openai" && provider != "codex" {
        return None;
    }
    OPENAI_RESET_HEADERS.iter().find_map(|name| {
        let value = headers.get(*name)?.to_str().ok()?.trim();
        parse_openai_reset_value(value)
    })
}

fn parse_openai_reset_value(value: &str) -> Option<u64> {
    if let Ok(secs) = value.parse::<u64>() {
        return Some(secs);
    }
    if let Some(stripped) = value.strip_suffix("ms") {
        let ms: u64 = stripped.trim().parse().ok()?;
        return Some(ms.div_ceil(1000));
    }
    if let Some(stripped) = value.strip_suffix('s') {
        let secs: u64 = stripped.trim().parse().ok()?;
        return Some(secs);
    }
    retry_after_http_date_secs(value)
}

fn retry_after_http_date_secs(value: &str) -> Option<u64> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.len() != 6 {
        return None;
    }
    let day: u32 = parts[1].parse().ok()?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i32 = parts[3].parse().ok()?;
    let time: Vec<&str> = parts[4].split(':').collect();
    if time.len() != 3 || parts[5] != "GMT" {
        return None;
    }
    let hour: u32 = time[0].parse().ok()?;
    let minute: u32 = time[1].parse().ok()?;
    let second: u32 = time[2].parse().ok()?;

    let days = days_from_civil(year, month, day)?;
    let target = days
        .checked_mul(86_400)?
        .checked_add(hour as i64 * 3600)?
        .checked_add(minute as i64 * 60)?
        .checked_add(second as i64)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(target.saturating_sub(now).max(0) as u64)
}

fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = month as i32 + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe - 719468) as i64)
}

fn jittered_backoff_secs(attempt: u8) -> u64 {
    let exp = 1u64 << attempt.saturating_sub(1);
    let base = exp.min(MAX_RETRY_DELAY_SECS);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    let jitter = nanos % (base + 1);
    jitter.max(1)
}

async fn send_retry_event(tx: &mpsc::Sender<Event>, provider: &str, delay_secs: u64, attempt: u8) {
    let _ = tx
        .send(Event::ProviderRetry {
            provider: provider.to_owned(),
            delay_secs,
            attempt,
            max_attempts: MAX_RETRIES,
        })
        .await;
}

fn extract_error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v["error"]["message"]
                .as_str()
                .or_else(|| v["message"].as_str())
                .or_else(|| v["error"].as_str())
                .map(|s| s.to_owned())
        })
        .unwrap_or_else(|| body[..body.len().min(200)].to_owned())
}

/// Send an HTTP request with retry/backoff for transient provider errors.
pub async fn send_with_retry<F, Fut>(
    provider: &str,
    tx: &mpsc::Sender<Event>,
    cancel: &CancellationToken,
    mut send: F,
) -> Result<reqwest::Response>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    for attempt in 1..=MAX_RETRIES {
        let resp = match send().await {
            Ok(r) => r,
            Err(e) => {
                bail!(format_network_error(&e));
            }
        };
        if resp.status().is_success() {
            return Ok(resp);
        }

        let status = resp.status();
        let retry_after = retry_after_secs(resp.headers())
            .or_else(|| provider_reset_secs(provider, resp.headers()));
        let body = resp.text().await.unwrap_or_default();
        let msg = extract_error_message(&body);
        let retryable = status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::BAD_GATEWAY
            || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
            || status == reqwest::StatusCode::GATEWAY_TIMEOUT
            || status.as_u16() == 529;

        if !retryable
            || attempt == MAX_RETRIES
            || (status == reqwest::StatusCode::TOO_MANY_REQUESTS && is_hard_quota_error(&msg))
        {
            bail!(format_http_error(provider, status, &msg));
        }

        let delay_secs = retry_after
            .unwrap_or_else(|| jittered_backoff_secs(attempt))
            .min(MAX_RETRY_DELAY_SECS);
        send_retry_event(tx, provider, delay_secs, attempt).await;
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(delay_secs)) => {}
            _ = cancel.cancelled() => bail!("Aborted"),
        }
    }
    bail!("request failed before stream start")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_429_with_guidance() {
        let msg = format_http_error(
            "claude",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "quota exceeded",
        );
        assert!(msg.contains("hard quota exceeded (429)"));
        assert!(msg.contains("Quota/billing must recover"));
    }

    #[test]
    fn formats_temporary_throttling_with_guidance() {
        let msg = format_http_error(
            "claude",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "too many requests",
        );
        assert!(msg.contains("temporary throttling (429)"));
        assert!(msg.contains("switch model/provider"));
    }

    #[test]
    fn detects_hard_quota_errors() {
        assert!(is_hard_quota_error("insufficient_quota"));
        assert!(is_hard_quota_error("quota exceeded"));
        assert!(!is_hard_quota_error("too many requests"));
    }

    #[test]
    fn parses_retry_after_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "12".parse().unwrap());
        assert_eq!(retry_after_secs(&headers), Some(12));
    }

    #[test]
    fn parses_retry_after_http_date() {
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        let secs = future
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let value = format_http_date(secs as i64);
        let parsed = retry_after_http_date_secs(&value).unwrap();
        assert!(parsed <= 60 && parsed > 0);
    }

    #[test]
    fn parses_openai_reset_seconds() {
        assert_eq!(parse_openai_reset_value("17"), Some(17));
        assert_eq!(parse_openai_reset_value("17s"), Some(17));
    }

    #[test]
    fn parses_openai_reset_millis() {
        assert_eq!(parse_openai_reset_value("1500ms"), Some(2));
    }

    #[test]
    fn picks_openai_reset_headers() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-reset-requests", "9s".parse().unwrap());
        assert_eq!(provider_reset_secs("openai", &headers), Some(9));
        assert_eq!(provider_reset_secs("codex", &headers), Some(9));
        assert_eq!(provider_reset_secs("claude", &headers), None);
    }

    fn format_http_date(secs: i64) -> String {
        const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let days = secs.div_euclid(86_400);
        let sod = secs.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        let hour = sod / 3600;
        let minute = (sod % 3600) / 60;
        let second = sod % 60;
        let weekday = WEEKDAYS[((days + 4).rem_euclid(7)) as usize];
        format!(
            "{weekday}, {day:02} {} {year:04} {hour:02}:{minute:02}:{second:02} GMT",
            MONTHS[(month - 1) as usize]
        )
    }

    fn civil_from_days(days: i64) -> (i32, u32, u32) {
        let z = days + 719468;
        let era = if z >= 0 { z } else { z - 146096 } / 146097;
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = mp + if mp < 10 { 3 } else { -9 };
        ((y + if m <= 2 { 1 } else { 0 }) as i32, m as u32, d as u32)
    }

    #[test]
    fn formats_401_with_auth_guidance() {
        let msg = format_http_error("claude", reqwest::StatusCode::UNAUTHORIZED, "invalid token");
        assert!(msg.contains("auth failed (401)"));
        assert!(msg.contains("luma sync"));
    }

    #[test]
    fn formats_403_with_permission_guidance() {
        let msg = format_http_error("openai", reqwest::StatusCode::FORBIDDEN, "access denied");
        assert!(msg.contains("access denied (403)"));
        assert!(msg.contains("permissions"));
    }

    #[test]
    fn formats_529_overloaded() {
        let status = reqwest::StatusCode::from_u16(529).unwrap();
        let msg = format_http_error("claude", status, "overloaded");
        assert!(msg.contains("overloaded (529)"));
        assert!(msg.contains("retry shortly"));
    }

    #[test]
    fn formats_unknown_status_as_raw() {
        let msg = format_http_error(
            "openai",
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "something broke",
        );
        assert!(msg.contains("500"));
        assert!(msg.contains("something broke"));
    }
}
