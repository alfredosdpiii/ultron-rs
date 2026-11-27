//! OpenRouter OAuth (`auth/oauth/openrouter.ts`): a PKCE login whose authorization code is exchanged for a
//! permanent, user-controlled API key. A one-shot loopback callback server on a system-chosen port races the paste
//! prompt, so a browser on another machine can still finish by pasting the redirect URL.
//!
//! Evidence: `fixtures/generate-oauth-openrouter.mjs` records the TypeScript flow; `tests/oauth_openrouter.rs` replays
//! it.

use crate::auth::{AuthEvent, AuthInteraction, ModelAuth, OAuthAuth, OAuthCredential};
use crate::http::{AbortSignal, BoxFuture, HttpRequest, Transport, cancelled, is_aborted};
use crate::oauth_flow::{
    TextResponse, api_key_model_auth, fetch_text, form_urlencode, generate_pkce, oauth_callback_host, oauth_error_html,
    oauth_redirect_host, oauth_success_html, start_browser_auth_prompt,
};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
const TOKEN_URL: &str = "https://openrouter.ai/api/v1/auth/keys";
const LOGIN_TIMEOUT: Duration = Duration::from_mins(5);
const TOKEN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const CANCELLED: &str = "Login cancelled";
/// `Number.MAX_SAFE_INTEGER`: the key never expires.
const NEVER: f64 = 9_007_199_254_740_991.0;

pub struct OpenRouterOAuth {
    pub transport: Arc<dyn Transport>,
}

/// `parseAuthorizationInput`: a pasted redirect URL's `code`, a query string's, or the bare value.
fn parse_authorization_input(input: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    let code_of = |url: &reqwest::Url| {
        url.query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.into_owned())
    };
    if let Ok(url) = reqwest::Url::parse(value) {
        return code_of(&url);
    }
    if value.contains("code=") {
        return reqwest::Url::parse(&format!("http://x/?{value}"))
            .ok()
            .and_then(|url| code_of(&url));
    }
    Some(value.to_string())
}

fn error_detail(body: &Map<String, Value>) -> Option<String> {
    for key in ["error_description", "message", "error"] {
        if let Some(text) = body.get(key).and_then(Value::as_str) {
            return Some(text.to_string());
        }
    }
    body.get("error")
        .and_then(Value::as_object)
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn parse_exchange_response(response: &TextResponse) -> Result<OAuthCredential, String> {
    let body = match serde_json::from_str::<Value>(&response.text) {
        Ok(Value::Object(body)) => body,
        Ok(_) => Map::new(),
        Err(_) if response.ok() => return Err("OpenRouter OAuth returned invalid JSON".into()),
        Err(_) => Map::new(),
    };
    if !response.ok() {
        let detail = error_detail(&body).map_or_default(|d| format!(": {d}"));
        return Err(format!(
            "OpenRouter OAuth key exchange failed (HTTP {}){detail}",
            response.status
        ));
    }
    let Some(key) = body.get("key").and_then(Value::as_str).filter(|k| !k.is_empty()) else {
        return Err("OpenRouter OAuth response carries no \"key\"".into());
    };
    Ok(OAuthCredential {
        access: key.to_string(),
        refresh: String::new(),
        expires: NEVER,
        extra: Map::new(),
    })
}

/// `exchangeAuthorizationCode`: the code and verifier for a key.
async fn exchange(
    transport: &dyn Transport,
    code: &str,
    verifier: &str,
    signal: Option<&AbortSignal>,
) -> Result<OAuthCredential, String> {
    if is_aborted(signal) {
        return Err(CANCELLED.into());
    }
    let request = HttpRequest {
        method: "POST".into(),
        url: TOKEN_URL.into(),
        headers: [
            ("accept".to_string(), "application/json".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ]
        .into_iter()
        .collect(),
        body: json!({"code": code, "code_verifier": verifier, "code_challenge_method": "S256"}).to_string(),
    };
    let response = match fetch_text(transport, request, signal, Some(TOKEN_EXCHANGE_TIMEOUT)).await {
        Ok(response) => response,
        Err(_) if is_aborted(signal) => return Err(CANCELLED.into()),
        Err(crate::oauth_flow::FetchError::Timeout) => {
            return Err("OpenRouter OAuth token exchange timed out".into());
        }
        Err(error) => return Err(error.message()),
    };
    parse_exchange_response(&response)
}

/// What the callback server settled the login with: a credential, `None` (handed over to the paste prompt), or an
/// error.
type Settled = Result<Option<OAuthCredential>, String>;

#[derive(Default)]
struct CallbackState {
    claimed: bool,
    settled: bool,
    sender: Option<tokio::sync::oneshot::Sender<Settled>>,
}

impl CallbackState {
    fn finish(&mut self, result: Settled) {
        if self.settled {
            return;
        }
        self.settled = true;
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(result);
        }
    }
}

fn http_status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Bad Gateway",
    }
}

async fn exchange_callback_code(
    code: &str,
    shared: &Arc<Mutex<CallbackState>>,
    verifier: &str,
    transport: &dyn Transport,
    signal: Option<&AbortSignal>,
) -> (u16, String) {
    jsrt::sync::lock(shared).claimed = true;
    match exchange(transport, code, verifier, signal).await {
        Ok(credential) => {
            jsrt::sync::lock(shared).finish(Ok(Some(credential)));
            (
                200,
                oauth_success_html("Signed in to OpenRouter. You may now close this page."),
            )
        }
        Err(error) => {
            let page = oauth_error_html("OpenRouter key exchange failed.", Some(&error));
            jsrt::sync::lock(shared).finish(Err(error));
            (502, page)
        }
    }
}

async fn handle_callback_request(
    url: &reqwest::Url,
    shared: &Arc<Mutex<CallbackState>>,
    verifier: &str,
    transport: &dyn Transport,
    signal: Option<&AbortSignal>,
) -> (u16, String) {
    let param = |key: &str| url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned());
    let already_used = {
        let guard = jsrt::sync::lock(shared);
        guard.claimed || guard.settled
    };
    if already_used {
        return (
            409,
            oauth_error_html("This OAuth callback has already been used.", None),
        );
    }
    if let Some(error) = param("error") {
        let description = param("error_description").unwrap_or(error);
        jsrt::sync::lock(shared).finish(Err(format!("OpenRouter authorization failed: {description}")));
        return (
            400,
            oauth_error_html("OpenRouter authorization was denied.", Some(&description)),
        );
    }
    if let Some(code) = param("code") {
        return exchange_callback_code(&code, shared, verifier, transport, signal).await;
    }
    (
        400,
        oauth_error_html("OpenRouter returned no authorization code.", None),
    )
}

async fn serve_callback_connection(
    mut socket: tokio::net::TcpStream,
    base: &str,
    shared: &Arc<Mutex<CallbackState>>,
    verifier: &str,
    transport: &dyn Transport,
    signal: Option<&AbortSignal>,
) -> bool {
    let head = crate::oauth_flow::read_http_head(&mut socket).await;
    let mut parts = head.split_whitespace();
    let (method, target) = (parts.next().unwrap_or("GET"), parts.next().unwrap_or("/"));
    let url = reqwest::Url::parse(base).and_then(|b| b.join(target));
    let expected = reqwest::Url::parse(base).map_or_default(|b| b.path().to_string());
    let (status, html) = match url {
        Ok(url) if method == "GET" && url.path() == expected => {
            handle_callback_request(&url, shared, verifier, transport, signal).await
        }
        _ => (404, oauth_error_html("OAuth callback route not found.", None)),
    };
    let reason = http_status_reason(status);
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
    jsrt::sync::lock(shared).settled
}

/// The one-shot loopback callback server (`startCallbackServer`).
struct CallbackServer {
    url: String,
    state: Arc<Mutex<CallbackState>>,
    receiver: tokio::sync::oneshot::Receiver<Settled>,
    stop: tokio::sync::oneshot::Sender<()>,
}

impl CallbackServer {
    async fn start(
        path: String,
        verifier: String,
        transport: Arc<dyn Transport>,
        signal: Option<AbortSignal>,
    ) -> Result<Self, String> {
        let host = oauth_callback_host();
        let listener = tokio::net::TcpListener::bind((host.as_str(), 0))
            .await
            .map_err(|e| e.to_string())?;
        let port = listener
            .local_addr()
            .map_err(|_| "Could not determine the OpenRouter OAuth callback port".to_string())?
            .port();
        let url = format!("http://{}:{port}{path}", oauth_redirect_host(&host));
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let state = Arc::new(Mutex::new(CallbackState {
            sender: Some(sender),
            ..CallbackState::default()
        }));
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let shared = state.clone();
        let base = url.clone();
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    accepted = listener.accept() => accepted,
                    _ = &mut stopped => return,
                };
                let Ok((socket, _)) = accepted else { continue };
                if serve_callback_connection(socket, &base, &shared, &verifier, transport.as_ref(), signal.as_ref())
                    .await
                {
                    return;
                }
            }
        });
        Ok(CallbackServer {
            url,
            state,
            receiver,
            stop,
        })
    }
}

async fn resolve_login_outcome(
    settled: Settled,
    manual_result: Option<Result<String, String>>,
    manual: std::pin::Pin<&mut BoxFuture<'_, Result<String, String>>>,
    interaction: &dyn AuthInteraction,
    transport: &dyn Transport,
    verifier: &str,
    signal: Option<&AbortSignal>,
) -> Result<OAuthCredential, String> {
    if let Some(Err(error)) = &manual_result {
        return Err(error.clone());
    }
    if let Some(credential) = settled? {
        return Ok(credential);
    }
    let input = match manual_result {
        Some(input) => input?,
        None => manual.await?,
    };
    let Some(code) = parse_authorization_input(&input) else {
        return Err("Missing authorization code".into());
    };
    interaction.notify(AuthEvent::Progress {
        message: "Exchanging authorization code for an API key...".into(),
    });
    exchange(transport, &code, verifier, signal).await
}

impl OpenRouterOAuth {
    async fn login_flow(&self, interaction: &dyn AuthInteraction) -> Result<OAuthCredential, String> {
        let signal = interaction.signal();
        let (verifier, challenge) = generate_pkce();
        let path = format!("/oauth/callback/{}", jsrt::random::uuid_v4());
        if is_aborted(signal.as_ref()) {
            return Err(CANCELLED.into());
        }
        let server = CallbackServer::start(path, verifier.clone(), self.transport.clone(), signal.clone()).await?;
        let callback = server.url.clone();
        let mut authorize = reqwest::Url::parse(AUTHORIZE_URL).map_err(|e| e.to_string())?;
        authorize.set_query(Some(&form_urlencode(&[
            ("callback_url", &callback),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ])));
        let (manual_abort, manual) = start_browser_auth_prompt(
            interaction,
            format!("Listening for OpenRouter OAuth callback on {callback}"),
            authorize.to_string(),
            "Complete sign-in in your browser. If the browser is on another machine, paste the final redirect URL here.",
            callback,
        );
        tokio::pin!(manual);
        let CallbackServer {
            state,
            mut receiver,
            stop,
            ..
        } = server;
        let mut manual_result: Option<Result<String, String>> = None;
        let settled = tokio::select! {
            settled = &mut receiver => settled.unwrap_or(Ok(None)),
            input = manual.as_mut() => {
                manual_result = Some(input);
                {
                    let mut guard = jsrt::sync::lock(&state);
                    if !guard.claimed {
                        guard.finish(Ok(None));
                    }
                }
                (&mut receiver).await.unwrap_or(Ok(None))
            }
            () = cancelled(signal.as_ref()) => Err(CANCELLED.into()),
            () = tokio::time::sleep(LOGIN_TIMEOUT) => Err("OpenRouter OAuth login timed out".into()),
        };
        let outcome = resolve_login_outcome(
            settled,
            manual_result,
            manual,
            interaction,
            self.transport.as_ref(),
            &verifier,
            signal.as_ref(),
        )
        .await;
        manual_abort.abort();
        let _ = stop.send(());
        outcome
    }
}

impl OAuthAuth for OpenRouterOAuth {
    fn name(&self) -> &str {
        "OpenRouter OAuth"
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with OpenRouter")
    }

    fn login<'a>(&'a self, interaction: &'a dyn AuthInteraction) -> BoxFuture<'a, Result<OAuthCredential, String>> {
        Box::pin(self.login_flow(interaction))
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        _signal: Option<AbortSignal>,
    ) -> BoxFuture<'a, Result<OAuthCredential, String>> {
        Box::pin(async move { Ok(credential.clone()) })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, String> {
        api_key_model_auth(credential)
    }
}
