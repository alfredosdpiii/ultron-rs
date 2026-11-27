//! Shared pieces of every OAuth flow (`auth/oauth/device-code.ts`, `pkce.ts`, `callback-host.ts`, `oauth-page.ts`):
//! RFC 8628 device-code polling, PKCE, the callback host, the browser result pages, the login clock/timer/randomness
//! ([`LoginEnv`]), and form/JSON requests over the crate's [`Transport`].

use crate::env::provider_env_value;
use crate::http::{AbortSignal, BoxFuture, HttpRequest, HttpResponse, Transport, cancelled, is_aborted};
use crate::jsutil::Utf8Stream;
use serde_json::Value;
use std::time::Duration;

use config::browser::{LEGACY_OAUTH_CALLBACK_HOST_ENV, OAUTH_CALLBACK_HOST_ENV};

const LOGO_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 800 800" aria-hidden="true"><path fill="#fff" fill-rule="evenodd" d="M165.29 165.29 H517.36 V400 H400 V517.36 H282.65 V634.72 H165.29 Z M282.65 282.65 V400 H400 V282.65 Z"/><path fill="#fff" d="M517.36 400 H634.72 V634.72 H517.36 Z"/></svg>"##;

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn render_page(title: &str, heading: &str, message: &str, details: Option<&str>) -> String {
    let title = escape_html(title);
    let heading = escape_html(heading);
    let message = escape_html(message);
    let details = details
        .filter(|d| !d.is_empty())
        .map_or_default(|d| format!("<div class=\"details\">{}</div>", escape_html(d)));
    let logo = LOGO_SVG;
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>{title}</title>
  <style>
    :root {{
      --text: #fafafa;
      --text-dim: #a1a1aa;
      --page-bg: #09090b;
      --font-sans: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, "Noto Sans", sans-serif, "Apple Color Emoji", "Segoe UI Emoji", "Segoe UI Symbol", "Noto Color Emoji";
      --font-mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace;
    }}
    * {{ box-sizing: border-box; }}
    html {{ color-scheme: dark; }}
    body {{
      margin: 0;
      min-height: 100vh;
      display: flex;
      align-items: center;
      justify-content: center;
      padding: 24px;
      background: var(--page-bg);
      color: var(--text);
      font-family: var(--font-sans);
      text-align: center;
    }}
    main {{
      width: 100%;
      max-width: 560px;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
    }}
    .logo {{
      width: 72px;
      height: 72px;
      display: block;
      margin-bottom: 24px;
    }}
    h1 {{
      margin: 0 0 10px;
      font-size: 28px;
      line-height: 1.15;
      font-weight: 650;
      color: var(--text);
    }}
    p {{
      margin: 0;
      line-height: 1.7;
      color: var(--text-dim);
      font-size: 15px;
    }}
    .details {{
      margin-top: 16px;
      font-family: var(--font-mono);
      font-size: 13px;
      color: var(--text-dim);
      white-space: pre-wrap;
      word-break: break-word;
    }}
  </style>
</head>
<body>
  <main>
    <div class="logo">{logo}</div>
    <h1>{heading}</h1>
    <p>{message}</p>
    {details}
  </main>
</body>
</html>"#
    )
}

pub fn oauth_success_html(message: &str) -> String {
    render_page("Authentication successful", "Authentication successful", message, None)
}

pub fn oauth_error_html(message: &str, details: Option<&str>) -> String {
    render_page("Authentication failed", "Authentication failed", message, details)
}

const LOOPBACK: &str = "127.0.0.1";

/// `oauthCallbackHost`: loopback unless `ULTRON_OAUTH_CALLBACK_HOST` (or `PI_OAUTH_CALLBACK_HOST`) names another.
pub fn oauth_callback_host() -> String {
    provider_env_value(OAUTH_CALLBACK_HOST_ENV, None)
        .or_else(|| provider_env_value(LEGACY_OAUTH_CALLBACK_HOST_ENV, None))
        .unwrap_or_else(|| LOOPBACK.into())
}

/// `oauthRedirectHost`: a wildcard address is reached over loopback.
pub fn oauth_redirect_host(host: &str) -> String {
    if matches!(host, "0.0.0.0" | "::" | "[::]") {
        LOOPBACK.into()
    } else {
        host.into()
    }
}

/// `Date.now()` for credential expiry, replaceable in tests.
pub type Clock = fn() -> i64;

pub fn system_clock() -> i64 {
    jsrt::date::now_ms()
}

/// Clock, timers and randomness of a login (`Date.now`, `setTimeout`, `crypto.getRandomValues`), so flows replay
/// exactly against recorded TypeScript runs.
pub trait LoginEnv: Send + Sync {
    fn now_ms(&self) -> f64;
    fn sleep(&self, ms: f64) -> BoxFuture<'_, ()>;
    fn fill_random(&self, buf: &mut [u8]);
}

fn tokio_sleep(ms: f64) -> BoxFuture<'static, ()> {
    Box::pin(tokio::time::sleep(Duration::from_millis(ms.max(0.0) as u64)))
}

/// The real clock, tokio timers and the OS random source.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemLoginEnv;

impl LoginEnv for SystemLoginEnv {
    fn now_ms(&self) -> f64 {
        jsrt::date::now_ms() as f64
    }

    fn sleep(&self, ms: f64) -> BoxFuture<'_, ()> {
        tokio_sleep(ms)
    }

    fn fill_random(&self, buf: &mut [u8]) {
        jsrt::random::fill(buf);
    }
}

/// An injected [`Clock`] with tokio timers (which a paused test runtime advances) and the OS random source.
#[derive(Clone, Copy, Debug)]
pub struct ClockLoginEnv(pub Clock);

impl LoginEnv for ClockLoginEnv {
    fn now_ms(&self) -> f64 {
        (self.0)() as f64
    }

    fn sleep(&self, ms: f64) -> BoxFuture<'_, ()> {
        tokio_sleep(ms)
    }

    fn fill_random(&self, buf: &mut [u8]) {
        SystemLoginEnv.fill_random(buf);
    }
}

/// `generatePKCE` with the OS random source.
pub fn generate_pkce() -> (String, String) {
    generate_pkce_from(&SystemLoginEnv)
}

/// `generatePKCE`: a base64url verifier of 32 random bytes and its S256 challenge.
pub fn generate_pkce_from(env: &dyn LoginEnv) -> (String, String) {
    let mut bytes = [0u8; 32];
    env.fill_random(&mut bytes);
    let verifier = jsrt::base64::encode_url(bytes);
    let challenge = pkce_challenge(&verifier);
    (verifier, challenge)
}

pub fn pkce_challenge(verifier: &str) -> String {
    jsrt::base64::encode_url(jsrt::hash::sha256(verifier))
}

/// `application/x-www-form-urlencoded` (`URLSearchParams.toString()`).
pub fn form_urlencode(fields: &[(&str, &str)]) -> String {
    let encode = |text: &str| {
        let mut out = String::new();
        for byte in text.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => out.push(byte as char),
                b' ' => out.push('+'),
                other => out.push_str(&format!("%{other:02X}")),
            }
        }
        out
    };
    fields
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// An HTTP response read whole.
#[derive(Clone, Debug)]
pub struct TextResponse {
    pub status: u16,
    pub text: String,
}

impl TextResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

async fn read_all(mut response: HttpResponse) -> TextResponse {
    let mut decoder = Utf8Stream::default();
    let mut text = String::new();
    while let Ok(Some(chunk)) = response.body.next_chunk().await {
        text.push_str(&decoder.push(&chunk));
    }
    text.push_str(&decoder.finish());
    TextResponse {
        status: response.status,
        text,
    }
}

/// Send `request` and read the whole body; `Err` is a network failure or an abort (`"aborted"`).
pub async fn fetch_text(
    transport: &dyn Transport,
    request: HttpRequest,
    signal: Option<&AbortSignal>,
    timeout: Option<Duration>,
) -> Result<TextResponse, FetchError> {
    if is_aborted(signal) {
        return Err(FetchError::Aborted);
    }
    let limit = async {
        match timeout {
            Some(t) => tokio::time::sleep(t).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        response = transport.send(request) => match response {
            Ok(response) => Ok(read_all(response).await),
            Err(error) => Err(FetchError::Network(error)),
        },
        () = cancelled(signal) => Err(FetchError::Aborted),
        () = limit => Err(FetchError::Timeout),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum FetchError {
    Network(String),
    Aborted,
    Timeout,
}

impl FetchError {
    pub fn message(&self) -> String {
        match self {
            FetchError::Network(message) => message.clone(),
            FetchError::Aborted => crate::provider::OPERATION_ABORTED.into(),
            FetchError::Timeout => crate::provider::OPERATION_TIMED_OUT.into(),
        }
    }
}

pub fn form_request(url: &str, fields: &[(&str, &str)], headers: &[(&str, &str)]) -> HttpRequest {
    HttpRequest {
        method: "POST".into(),
        url: url.into(),
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
            .collect(),
        body: form_urlencode(fields),
    }
}

pub(crate) const CANCEL_MESSAGE: &str = "Login cancelled";
const TIMEOUT_MESSAGE: &str = "Device flow timed out";
const SLOW_DOWN_TIMEOUT_MESSAGE: &str = "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";
const MINIMUM_INTERVAL_MS: f64 = 1000.0;
const DEFAULT_POLL_INTERVAL_SECONDS: f64 = 5.0;
const SLOW_DOWN_INTERVAL_INCREMENT_MS: f64 = 5000.0;

/// One poll's outcome (`OAuthDeviceCodePollResult`).
pub enum Poll<T> {
    Pending,
    SlowDown(Option<f64>),
    Failed(String),
    Complete(T),
}

/// `abortableSleep`: sleeps on `env`'s timer unless `signal` aborts first.
pub async fn abortable_sleep(
    env: &dyn LoginEnv,
    ms: f64,
    signal: Option<&AbortSignal>,
    cancel_message: &str,
) -> Result<(), String> {
    if is_aborted(signal) {
        return Err(cancel_message.into());
    }
    tokio::select! {
        () = env.sleep(ms) => Ok(()),
        () = cancelled(signal) => Err(cancel_message.into()),
    }
}

fn next_slow_down_interval(current_ms: f64, server_seconds: Option<f64>) -> f64 {
    match server_seconds.filter(|s| s.is_finite() && *s > 0.0) {
        Some(seconds) => MINIMUM_INTERVAL_MS.max((seconds * 1000.0).floor()),
        None => MINIMUM_INTERVAL_MS.max(current_ms + SLOW_DOWN_INTERVAL_INCREMENT_MS),
    }
}

async fn sleep_until_deadline(
    env: &dyn LoginEnv,
    deadline: f64,
    interval_ms: f64,
    signal: Option<&AbortSignal>,
) -> Result<bool, String> {
    let remaining = deadline - env.now_ms();
    if remaining <= 0.0 {
        return Ok(false);
    }
    abortable_sleep(env, interval_ms.min(remaining), signal, CANCEL_MESSAGE).await?;
    Ok(true)
}

fn device_flow_timeout_message(slow_downs: u32) -> String {
    if slow_downs > 0 {
        SLOW_DOWN_TIMEOUT_MESSAGE.into()
    } else {
        TIMEOUT_MESSAGE.into()
    }
}

enum PollStep<T> {
    Done(Result<T, String>),
    Continue { interval_ms: f64, slow_downs: u32 },
}

fn apply_poll_outcome<T>(outcome: Poll<T>, interval_ms: f64, slow_downs: u32) -> PollStep<T> {
    match outcome {
        Poll::Complete(value) => PollStep::Done(Ok(value)),
        Poll::Failed(message) => PollStep::Done(Err(message)),
        Poll::SlowDown(server) => PollStep::Continue {
            interval_ms: next_slow_down_interval(interval_ms, server),
            slow_downs: slow_downs + 1,
        },
        Poll::Pending => PollStep::Continue {
            interval_ms,
            slow_downs,
        },
    }
}

/// `pollOAuthDeviceCodeFlow`: polls until `poll` completes or fails, the deadline (`expires_in_seconds` from now on
/// `env`'s clock) passes, or `signal` aborts. Polls immediately unless `wait_before_first_poll`; RFC 8628 interval
/// rules: default 5 s, at least 1 s, `slow_down` takes the server's interval or adds 5 s.
pub async fn poll_device_code_flow<T, F, Fut>(
    env: &dyn LoginEnv,
    signal: Option<&AbortSignal>,
    interval_seconds: Option<f64>,
    expires_in_seconds: Option<f64>,
    wait_before_first_poll: bool,
    mut poll: F,
) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Poll<T>, String>>,
{
    let deadline = expires_in_seconds.map_or(f64::INFINITY, |s| env.now_ms() + s * 1000.0);
    let mut interval_ms =
        MINIMUM_INTERVAL_MS.max((interval_seconds.unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS) * 1000.0).floor());
    let mut slow_downs = 0u32;
    if wait_before_first_poll {
        let _ = sleep_until_deadline(env, deadline, interval_ms, signal).await?;
    }
    while env.now_ms() < deadline {
        if is_aborted(signal) {
            return Err(CANCEL_MESSAGE.into());
        }
        match apply_poll_outcome(poll().await?, interval_ms, slow_downs) {
            PollStep::Done(result) => return result,
            PollStep::Continue {
                interval_ms: next_interval,
                slow_downs: next_slow_downs,
            } => {
                interval_ms = next_interval;
                slow_downs = next_slow_downs;
            }
        }
        if !sleep_until_deadline(env, deadline, interval_ms, signal).await? {
            break;
        }
    }
    Err(device_flow_timeout_message(slow_downs))
}

/// `new URL(text)` accepting only http(s) (`trustedHttpUrl`), normalized (`url.href`).
pub fn trusted_http_url(value: Option<&Value>, https_only: bool) -> Option<String> {
    let text = value?.as_str().filter(|s| !s.is_empty())?;
    let url = reqwest::Url::parse(text).ok()?;
    let ok = url.scheme() == "https" || (!https_only && url.scheme() == "http");
    ok.then(|| url.to_string())
}

/// Read an HTTP/1.x request head (up to `\r\n\r\n`) from a local OAuth callback socket.
pub(crate) async fn read_http_head(socket: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// Notify the user of a browser OAuth callback listener and authorization URL, and start the manual fallback prompt.
pub(crate) fn start_browser_auth_prompt<'a>(
    interaction: &'a dyn crate::auth::AuthInteraction,
    progress_message: String,
    authorize_url: String,
    instructions: &str,
    placeholder: String,
) -> (AbortSignal, BoxFuture<'a, Result<String, String>>) {
    interaction.notify(crate::auth::AuthEvent::Progress {
        message: progress_message,
    });
    interaction.notify(crate::auth::AuthEvent::AuthUrl {
        url: authorize_url,
        instructions: Some(instructions.into()),
    });
    let manual_abort = AbortSignal::new();
    let manual = interaction.prompt(
        crate::auth::AuthPrompt::ManualCode {
            message: "Complete sign-in in your browser, or paste the authorization code / redirect URL here:".into(),
            placeholder: Some(placeholder),
        },
        Some(manual_abort.clone()),
    );
    (manual_abort, manual)
}

/// Convert an OAuth credential's access token into a [`crate::auth::ModelAuth`] carrying an API key.
pub(crate) fn api_key_model_auth(credential: &crate::auth::OAuthCredential) -> Result<crate::auth::ModelAuth, String> {
    Ok(crate::auth::ModelAuth {
        api_key: Some(credential.access.clone()),
        ..Default::default()
    })
}

/// Map a failed login result to [`CANCEL_MESSAGE`] when the abort signal has fired.
pub(crate) fn map_aborted_login(
    result: Result<crate::auth::OAuthCredential, String>,
    signal: Option<&AbortSignal>,
) -> Result<crate::auth::OAuthCredential, String> {
    match result {
        Err(_) if is_aborted(signal) => Err(CANCEL_MESSAGE.into()),
        other => other,
    }
}
