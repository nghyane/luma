//! PKCE primitives shared across OAuth providers.

use anyhow::{Context as _, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const LOGIN_TIMEOUT_SECS: u64 = 300;

pub struct CallbackPayload {
    pub code: Option<String>,
    pub state: String,
    pub login_option: Option<String>,
    pub issuer_url: Option<String>,
    pub idc_region: Option<String>,
}

pub fn gen_verifier() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("system entropy unavailable");
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn gen_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn gen_state() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("system entropy unavailable");
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn decode_jwt_payload(token: &str) -> Option<serde_json::Value> {
    let segment = token.split('.').nth(1)?;
    let padded = match segment.len() % 4 {
        2 => format!("{segment}=="),
        3 => format!("{segment}="),
        _ => segment.to_owned(),
    };
    let decoded = padded.replace('-', "+").replace('_', "/");
    let bytes = base64_decode(&decoded)?;
    serde_json::from_slice(&bytes).ok()
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let bytes: Vec<u8> = input.bytes().filter(|&b| b != b'=').collect();
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &b) in chunk.iter().enumerate() {
            let val = TABLE.iter().position(|&c| c == b)? as u32;
            n |= val << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

pub async fn bind_loopback(port: u16) -> Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| {
            if port == 0 {
                "could not bind loopback listener".to_owned()
            } else {
                format!("could not bind loopback listener on port {port}")
            }
        })
}

pub async fn accept_callback(
    listener: tokio::net::TcpListener,
    expected_path: &str,
) -> Result<CallbackPayload> {
    accept_callback_any(listener, &[expected_path]).await
}

pub async fn accept_callback_any(
    listener: tokio::net::TcpListener,
    expected_paths: &[&str],
) -> Result<CallbackPayload> {
    loop {
        let (mut stream, _) = listener.accept().await.context("callback accept failed")?;
        match read_callback_request(&mut stream, expected_paths).await {
            Ok(Some(payload)) => {
                let _ = stream.write_all(SUCCESS_RESPONSE.as_bytes()).await;
                let _ = stream.shutdown().await;
                return Ok(payload);
            }
            _ => {
                let _ = stream
                    .write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n")
                    .await;
                let _ = stream.shutdown().await;
            }
        }
    }
}

async fn read_callback_request(
    stream: &mut tokio::net::TcpStream,
    expected_paths: &[&str],
) -> Result<Option<CallbackPayload>> {
    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(None);
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let first_line = req.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    if parts.next() != Some("GET") {
        return Ok(None);
    }
    let target = parts.next().unwrap_or("");
    let Some((path, query)) = target.split_once('?') else {
        return Ok(None);
    };
    if !expected_paths.contains(&path) {
        return Ok(None);
    }

    let mut code = None;
    let mut state = None;
    let mut login_option = None;
    let mut issuer_url = None;
    let mut idc_region = None;
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        match k {
            "code" => code = Some(url_decode(v)),
            "state" => state = Some(url_decode(v)),
            "login_option" => login_option = Some(url_decode(v)),
            "issuer_url" => issuer_url = Some(url_decode(v)),
            "idc_region" => idc_region = Some(url_decode(v)),
            _ => {}
        }
    }

    match state {
        Some(state) => Ok(Some(CallbackPayload {
            code,
            state,
            login_option,
            issuer_url,
            idc_region,
        })),
        _ => Ok(None),
    }
}

const SUCCESS_RESPONSE: &str = concat!(
    "HTTP/1.1 200 OK\r\n",
    "Content-Type: text/html; charset=utf-8\r\n",
    "Connection: close\r\n\r\n",
    "<!doctype html><html><head><meta charset=\"utf-8\"><title>luma · signed in</title></head>",
    "<body><h1>Signed in</h1><p>You can close this tab and return to luma.</p></body></html>",
);

pub fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .context("failed to open browser")?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .context("failed to open browser")?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
            .context("failed to open browser")?;
    }
    Ok(())
}

pub async fn exchange_json_token(
    url: &str,
    body: String,
    extra_headers: &[(&str, &str)],
) -> Result<serde_json::Value> {
    post_token(url, "application/json", body, extra_headers).await
}

pub async fn exchange_form_token(url: &str, body: String) -> Result<serde_json::Value> {
    post_token(url, "application/x-www-form-urlencoded", body, &[]).await
}

async fn post_token(
    url: &str,
    content_type: &str,
    body: String,
    extra_headers: &[(&str, &str)],
) -> Result<serde_json::Value> {
    let client = reqwest::Client::new();
    let mut req = client
        .post(url)
        .header("Content-Type", content_type)
        .header("Accept", "application/json")
        .body(body);
    for (k, v) in extra_headers {
        req = req.header(*k, *v);
    }
    let res = req.send().await.context("token exchange network error")?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "token exchange HTTP {status}: {}",
            crate::util::byte_prefix(&text, 300)
        );
    }
    serde_json::from_str(&text).context("bad token exchange json")
}
