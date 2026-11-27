//! `ultron auth check|print-api-key|print-bearer-token` (`cli/auth-command.ts`, `cli/auth-check.ts`,
//! `cli/credential-print.ts`): whether a provider has usable credentials, and the credential itself.
//!
//! Credentials come from `auth.json` (an `api_key` or an `oauth` entry) and the provider's environment variables, as
//! `resolveProviderAuth` reads them, through the session's request auth: built-in and models.json providers, keys
//! from auth.json, models.json (literal, `$VAR`, `!command`) or the environment, and stored OAuth tokens refreshed
//! through the provider (Anthropic, OpenAI Codex, GitHub Copilot, xAI, Kimi For Coding, Meta, OpenRouter, Radius)
//! when they expire within the requested window. `--model` resolves as `resolveCliModel` does: under `--provider`,
//! or alone across the providers with stored credentials. The `claude-code` provider is configured when its CLI
//! exists (`claudeCodeCliAuth`, found on the run's PATH without running it). Not ported: extension providers (Pi
//! extensions are not supported by ultron-rs).

use crate::Env;
use crate::args::{Args, parse_args};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Check,
    ApiKey,
    BearerToken,
}

#[derive(Debug, PartialEq)]
pub struct AuthCommand {
    pub kind: Kind,
    pub args: Vec<String>,
    pub json: bool,
    pub credentials: bool,
    pub no_refresh: bool,
    pub min_expiry_ms: Option<f64>,
}

const HELP: &str = "Usage:
  pi auth print-api-key [--provider <provider>] [--model <model>]
  pi auth print-bearer-token [--provider <provider>] [--model <model>] [--min-expiry <duration>]
  pi auth check [--provider <provider>] [--model <model>] [--json] [--credentials] [--no-refresh]

Auth commands require at least one of --provider or --model. Checks refresh expired OAuth credentials by default; --no-refresh prevents this. --credentials emits the credential, or includes it in JSON output.";

const NO_API_KEY: [&str; 1] = ["openai-codex"];
const DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS: f64 = 30.0 * 60_000.0;
const MIN_OAUTH_VALIDITY_MS: f64 = 5.0 * 60_000.0;

fn name(kind: Kind) -> &'static str {
    match kind {
        Kind::Check => "auth check",
        Kind::ApiKey => "auth print-api-key",
        Kind::BearerToken => "auth print-bearer-token",
    }
}

fn usage(kind: Kind) -> &'static str {
    match kind {
        Kind::Check => "ultron auth check --provider <provider> [--json] [--credentials] [--no-refresh]",
        Kind::ApiKey => "ultron auth print-api-key --provider <provider> [--model <model>]",
        Kind::BearerToken => {
            "ultron auth print-bearer-token --provider <provider> [--model <model>] [--min-expiry <duration>]"
        }
    }
}

pub fn is_help(args: &[String]) -> bool {
    args.first().map(String::as_str) == Some("auth")
        && (args.get(1).is_none_or(|a| a == "help") || args.iter().any(|a| a == "--help" || a == "-h"))
}

fn parse_auth_kind(arg: Option<&str>) -> Result<Kind, String> {
    match arg {
        Some("check") => Ok(Kind::Check),
        Some("print-api-key") => Ok(Kind::ApiKey),
        Some("print-bearer-token") => Ok(Kind::BearerToken),
        other => Err(format!(
            "Unknown auth command \"{}\". Use \"ultron auth print-api-key\", \"ultron auth print-bearer-token\", or \"ultron auth check\".",
            other.unwrap_or("")
        )),
    }
}

fn apply_auth_arg(command: &mut AuthCommand, args: &[String], index: &mut usize) -> Result<(), String> {
    let arg = &args[*index];
    if arg == "--min-expiry" {
        if command.kind != Kind::BearerToken {
            return Err("--min-expiry is only supported by print-bearer-token".into());
        }
        *index += 1;
        let ms = args
            .get(*index)
            .and_then(|v| parse_duration(v))
            .ok_or_else(|| "--min-expiry must use a duration such as 30m or 1h".to_string())?;
        command.min_expiry_ms = Some(ms);
        return Ok(());
    }
    if matches!(arg.as_str(), "--json" | "--credentials" | "--no-refresh") {
        if command.kind != Kind::Check {
            return Err(format!("{arg} is only supported by auth check"));
        }
        match arg.as_str() {
            "--json" => command.json = true,
            "--credentials" => command.credentials = true,
            _ => command.no_refresh = true,
        }
        return Ok(());
    }
    command.args.push(arg.clone());
    Ok(())
}

/// `parseAuthCommand`: None when `args` is not an auth command.
pub fn parse_auth_command(args: &[String]) -> Option<Result<AuthCommand, String>> {
    if args.first().map(String::as_str) != Some("auth") {
        return None;
    }
    let kind = match parse_auth_kind(args.get(1).map(String::as_str)) {
        Ok(kind) => kind,
        Err(err) => return Some(Err(err)),
    };
    let mut command = AuthCommand {
        kind,
        args: Vec::new(),
        json: false,
        credentials: false,
        no_refresh: false,
        min_expiry_ms: None,
    };
    let mut index = 2;
    while index < args.len() {
        if let Err(err) = apply_auth_arg(&mut command, args, &mut index) {
            return Some(Err(err));
        }
        index += 1;
    }
    Some(Ok(command))
}

/// `/^(\d+)(ms|s|m|h)$/iu`. Units are compared case-sensitively afterwards, as the TypeScript does: `1H` matches
/// but multiplies by an hour only through the final branch.
fn parse_duration(value: &str) -> Option<f64> {
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    let (number, unit) = value.split_at(digits);
    if digits == 0 || !["ms", "s", "m", "h"].contains(&unit.to_ascii_lowercase().as_str()) {
        return None;
    }
    let amount: f64 = number.parse().ok()?;
    Some(
        amount
            * match unit {
                "ms" => 1.0,
                "s" => 1_000.0,
                "m" => 60_000.0,
                _ => 3_600_000.0,
            },
    )
}

/// `validateAuthCommandArgs`.
fn validate(args: &Args, kind: Kind) -> Result<(Option<String>, Option<String>), String> {
    let trimmed = |v: &Option<String>| {
        v.as_deref()
            .map(jsrt::string::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let (provider, model) = (trimmed(&args.provider), trimmed(&args.model));
    if let Some((option, _)) = args.unknown_flags.first() {
        return Err(format!("Unknown option --{option} for \"{}\".", name(kind)));
    }
    if args.api_key.is_some() || !args.messages.is_empty() || !args.file_args.is_empty() {
        return Err("Auth commands only accept --provider and --model".into());
    }
    if provider.is_none() && model.is_none() {
        return Err(if kind == Kind::Check {
            "Auth checks require --provider <provider> or --model <model>".into()
        } else {
            "Credential printing requires --provider <provider> or --model <model>".into()
        });
    }
    Ok((provider, model))
}

/// A credential that could not be read or resolved: a check reports the provider invalid, a print fails.
struct Unsupported;

struct Credentials<'a> {
    env: &'a Env,
    storage: config::auth::AuthStorage,
    /// How OAuth refreshes reach their provider.
    options: &'a session_core::stored_auth::OAuthOptions,
}

impl Credentials<'_> {
    /// `modelRuntime.getProvider`: a built-in provider, the Claude Code CLI, or one models.json defines.
    fn known(&self, provider: &str) -> bool {
        ai::models::builtin_providers().any(|p| p == provider)
            || provider == ai::claude_code::PROVIDER
            || session_core::lane::registry(self.env)
                .config()
                .provider(provider)
                .is_some()
    }

    /// The provider signs in with an account (`provider.auth.oauth`).
    fn oauth(&self, provider: &str) -> bool {
        session_core::stored_auth::oauth_handler_with(self.env, provider, self.options).is_some()
    }

    fn read(&mut self, provider: &str) -> Result<Option<Value>, Unsupported> {
        self.storage.read(provider).map_err(|_| Unsupported)
    }

    fn list(&mut self) -> Result<Vec<(String, Value)>, Unsupported> {
        self.storage.list().map_err(|_| Unsupported)
    }

    /// `getAuth`: the credential and its kind (`api_key` or `oauth`). A stored OAuth token is refreshed through the
    /// provider when it has less than `min_validity` (at least five minutes) left; otherwise the key the provider
    /// resolves (stored, models.json, the environment). A resolution that fails is [`Unsupported`].
    fn resolve(
        &mut self,
        provider: &str,
        min_validity: Option<f64>,
    ) -> Result<Option<(String, &'static str)>, Unsupported> {
        if provider == ai::claude_code::PROVIDER {
            return Ok(None);
        }
        let stored = self.read(provider)?;
        match stored.as_ref().and_then(|s| s.get("type")).and_then(Value::as_str) {
            Some("oauth") => {
                if !self.oauth(provider) {
                    return Ok(None);
                }
                let minimum = MIN_OAUTH_VALIDITY_MS.max(min_validity.unwrap_or(0.0));
                session_core::stored_auth::oauth_access_with(self.env, provider, Some(minimum), self.options)
                    .map(|access| access.map(|a| (a, "oauth")))
                    .map_err(|_| Unsupported)
            }
            Some("api_key") if NO_API_KEY.contains(&provider) => Ok(None),
            Some(other) if other != "api_key" => Ok(None),
            _ => session_core::lane::request_key(&session_core::ModelFlags::default(), self.env, provider)
                .map(|key| key.filter(|k| !k.is_empty()).map(|k| (k, "api_key")))
                .map_err(|_| Unsupported),
        }
    }

    /// `checkAuth`: whether the provider is configured, and how.
    fn check(&mut self, provider: &str) -> Result<Option<&'static str>, Unsupported> {
        if let Some(stored) = self.read(provider)?
            && stored.get("type").and_then(Value::as_str) == Some("oauth")
        {
            return Ok(self.oauth(provider).then_some("oauth"));
        }
        if provider == ai::claude_code::PROVIDER {
            return Ok(None);
        }
        let stored_key = self.read(provider)?.is_some_and(|s| {
            s.get("type").and_then(Value::as_str) == Some("api_key")
                && s.get("key").and_then(Value::as_str).is_some_and(|k| !k.is_empty())
        });
        let registry = session_core::lane::registry(self.env);
        let inherited = session_core::lane::env_api_key(self.env, provider).is_some();
        let configured = !NO_API_KEY.contains(&provider)
            && (stored_key || registry.auth_configured(provider, &*self.env.var, inherited));
        Ok(configured.then_some("api_key"))
    }
}

fn invalid_state(provider: &str) -> Value {
    json!({"status": "invalid", "provider": provider, "reason": "invalid_state"})
}

fn auth_file_invalid(command: &AuthCommand, path: &std::path::Path) -> bool {
    command.no_refresh
        && std::fs::read_to_string(path).is_ok_and(|content| config::auth::validate_auth_file(&content).is_err())
}

fn resolve_check_provider(
    env: &Env,
    provider: Option<String>,
    model: Option<String>,
    requested: String,
) -> Option<String> {
    let Some(model) = model else {
        return Some(requested);
    };
    let resolved = session_core::lane::resolve_cli_model(env, provider.as_deref(), &model, false);
    match resolved.model {
        Some(found) if resolved.error.is_none() => Some(found.provider),
        _ => None,
    }
}

fn check_claude_code(env: &Env, command: &AuthCommand, provider: &str) -> Value {
    let source = ai::env::EnvSource {
        lookup: &*env.var,
        exists: &|p: &std::path::Path| p.exists(),
        home: Some(env.home.clone()),
    };
    if !ai::claude_code::configured(&source) {
        json!({"status": "not_ready", "provider": provider, "reason": "credentials_not_configured"})
    } else if command.credentials {
        json!({"status": "not_ready", "provider": provider, "reason": "credential_not_available"})
    } else {
        json!({"status": "ready", "provider": provider, "authType": "api_key"})
    }
}

fn resolve_check_credential(
    credentials: &mut Credentials<'_>,
    command: &AuthCommand,
    provider: &str,
) -> Result<Option<String>, Unsupported> {
    let stored = credentials.read(provider)?;
    if command.no_refresh
        && let Some(s) = stored
        && s.get("type").and_then(Value::as_str) == Some("oauth")
    {
        return Ok(s.get("access").and_then(Value::as_str).map(str::to_string));
    }
    Ok(credentials
        .resolve(provider, None)?
        .map(|(c, _)| c)
        .filter(|c| c != ai::env::AMBIENT_CREDENTIALS))
}

fn check_stored_credential(
    credentials: &mut Credentials<'_>,
    command: &AuthCommand,
    provider: &str,
) -> Result<(Value, Option<String>), Unsupported> {
    let not_configured = json!({"status": "not_ready", "provider": provider, "reason": "credentials_not_configured"});
    let Some(auth_type) = credentials.check(provider)? else {
        return Ok((not_configured, None));
    };
    if !command.no_refresh && credentials.resolve(provider, None)?.is_none() {
        return Ok((not_configured, None));
    }
    let ready = json!({"status": "ready", "provider": provider, "authType": auth_type});
    if !command.credentials {
        return Ok((ready, None));
    }
    Ok(match resolve_check_credential(credentials, command, provider)? {
        Some(c) => (ready, Some(c)),
        None => (
            json!({"status": "not_ready", "provider": provider, "reason": "credential_not_available"}),
            None,
        ),
    })
}

fn check(
    command: &AuthCommand,
    parsed: &Args,
    env: &Env,
    options: &session_core::stored_auth::OAuthOptions,
) -> Result<(Value, Option<String>), String> {
    let (provider, model) = validate(parsed, Kind::Check)?;
    let requested = provider.clone().or_else(|| model.clone()).unwrap_or_default();
    let path = config::paths::AgentPaths::new(env.agent_dir()).auth();
    if auth_file_invalid(command, &path) {
        return Ok((invalid_state(&requested), None));
    }
    let mut credentials = Credentials {
        env,
        storage: config::auth::AuthStorage::create(path),
        options,
    };
    if credentials.list().is_err() {
        return Ok((invalid_state(&requested), None));
    }
    let Some(provider) = resolve_check_provider(env, provider, model, requested.clone()) else {
        return Ok((invalid_state(&requested), None));
    };
    if session_core::lane::registry(env).error().is_some() {
        return Ok((invalid_state(&provider), None));
    }
    if !credentials.known(&provider) {
        return Ok((
            json!({"status": "not_ready", "provider": provider, "reason": "provider_not_found"}),
            None,
        ));
    }
    if provider == ai::claude_code::PROVIDER {
        return Ok((check_claude_code(env, command, &provider), None));
    }
    let outcome = check_stored_credential(&mut credentials, command, &provider);
    Ok(outcome.unwrap_or_else(|Unsupported| (invalid_state(&provider), None)))
}

fn credential_type_of<'a>(types: &'a [(String, Value)], id: &str) -> Option<&'a str> {
    types.iter().find(|(p, _)| p == id).and_then(|(_, t)| t.as_str())
}

fn resolve_named_provider(
    env: &Env,
    credentials: &Credentials<'_>,
    provider: &str,
    model: Option<&str>,
) -> Result<Vec<String>, String> {
    if !credentials.known(provider) {
        return Err(format!(
            "Unknown provider \"{provider}\". Use --list-models to see available providers."
        ));
    }
    if let Some(model) = model {
        let resolved = session_core::lane::resolve_cli_model(env, Some(provider), model, true);
        if resolved.model.is_none() || resolved.error.is_some() {
            return Err(resolved
                .error
                .unwrap_or_else(|| "Unable to resolve the requested provider/model".into()));
        }
    }
    Ok(vec![provider.to_string()])
}

fn resolve_model_providers(env: &Env, model: &str, types: &[(String, Value)]) -> Result<Vec<String>, String> {
    let matched: Vec<String> = session_core::lane::registry(env)
        .provider_ids()
        .into_iter()
        .filter(|id| credential_type_of(types, id).is_some())
        .filter(|id| {
            let resolved = session_core::lane::resolve_cli_model(env, Some(id), model, true);
            resolved.model.is_some()
                && resolved.error.is_none()
                && !resolved
                    .warning
                    .as_deref()
                    .is_some_and(|w| w.contains("Using custom model id"))
        })
        .collect();
    if matched.is_empty() {
        return Err(format!(
            "Model \"{model}\" not found. Use --list-models to see available models."
        ));
    }
    Ok(matched)
}

fn collect_credentials(
    command: &AuthCommand,
    credentials: &mut Credentials<'_>,
    providers: &[String],
    types: &[(String, Value)],
) -> Result<Vec<(String, String)>, String> {
    let kind = command.kind;
    let min =
        (kind == Kind::BearerToken).then_some(command.min_expiry_ms.unwrap_or(DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS));
    let mut found = Vec::new();
    for id in providers {
        let is_oauth = credential_type_of(types, id) == Some("oauth");
        if (kind == Kind::ApiKey) == is_oauth {
            continue;
        }
        if let Some(value) = credentials
            .resolve(id, min)
            .map_err(|Unsupported| "Failed to resolve credential".to_string())?
            .map(|(c, _)| c)
            .filter(|c| c != ai::env::AMBIENT_CREDENTIALS)
        {
            found.push((id.clone(), value));
        }
    }
    Ok(found)
}

fn missing_credential_error(kind: Kind, named: bool, provider: &str, credential_type: Option<&str>) -> String {
    if named && kind == Kind::ApiKey && credential_type == Some("oauth") {
        return format!("Provider \"{provider}\" is configured with OAuth, not an API key");
    }
    if named && kind == Kind::BearerToken && credential_type != Some("oauth") {
        return format!("Provider \"{provider}\" is not configured with an OAuth bearer token");
    }
    format!(
        "No usable {} is configured",
        if kind == Kind::ApiKey {
            "API key"
        } else {
            "OAuth bearer token"
        }
    )
}

fn print_credential(
    command: &AuthCommand,
    parsed: &Args,
    env: &Env,
    options: &session_core::stored_auth::OAuthOptions,
) -> Result<String, String> {
    let kind = command.kind;
    let (provider, model) = validate(parsed, kind)?;
    let path = config::paths::AgentPaths::new(env.agent_dir()).auth();
    let mut credentials = Credentials {
        env,
        storage: config::auth::AuthStorage::create(path),
        options,
    };
    let types = credentials
        .list()
        .map_err(|Unsupported| "Failed to resolve credential".to_string())?;
    let providers = match provider.as_deref() {
        Some(p) => resolve_named_provider(env, &credentials, p, model.as_deref())?,
        None => resolve_model_providers(env, model.as_deref().unwrap_or_default(), &types)?,
    };
    let mut found = collect_credentials(command, &mut credentials, &providers, &types)?;
    if found.len() > 1 {
        let ids: Vec<&str> = found.iter().map(|(id, _)| id.as_str()).collect();
        return Err(format!(
            "Multiple configured providers matched ({}). Specify --provider.",
            ids.join(", ")
        ));
    }
    if let Some((_, value)) = found.pop() {
        return Ok(value);
    }
    let first = &providers[0];
    Err(missing_credential_error(
        kind,
        provider.is_some(),
        first,
        credential_type_of(&types, first),
    ))
}

/// Handle `auth ...`; None when `argv` is not an auth command, else the exit code.
pub fn run(argv: &[String], env: &Env, out: &mut dyn std::io::Write, err: &mut dyn std::io::Write) -> Option<i32> {
    run_with(
        argv,
        env,
        out,
        err,
        &session_core::stored_auth::OAuthOptions::for_env(env),
    )
}

fn write_print_outcome(
    outcome: Result<String, String>,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> i32 {
    match outcome {
        Ok(value) => {
            let _ = writeln!(out, "{value}");
            0
        }
        Err(message) => {
            let _ = writeln!(err, "Error: {message}");
            1
        }
    }
}

fn write_check_outcome(
    command: &AuthCommand,
    outcome: Result<(Value, Option<String>), String>,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> i32 {
    let (mut result, credential) = match outcome {
        Ok(ok) => ok,
        Err(message) => {
            let _ = writeln!(err, "Error: {message}");
            return 2;
        }
    };
    let status = result["status"].as_str().unwrap_or_default().to_string();
    let output = if command.json {
        if let Some(c) = &credential {
            result["credentials"] = Value::String(c.clone());
        }
        jsrt::json::stringify(&result)
    } else {
        credential.unwrap_or_else(|| status.clone())
    };
    let _ = writeln!(out, "{output}");
    match status.as_str() {
        "ready" => 0,
        "not_ready" => 1,
        _ => 2,
    }
}

/// [`run`] with OAuth refreshes going through `options` (tests answer the providers' endpoints).
pub fn run_with(
    argv: &[String],
    env: &Env,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
    options: &session_core::stored_auth::OAuthOptions,
) -> Option<i32> {
    if is_help(argv) {
        let _ = writeln!(out, "{HELP}");
        return Some(0);
    }
    let command = match parse_auth_command(argv)? {
        Ok(command) => command,
        Err(message) => {
            let _ = writeln!(err, "Error: {message}");
            return Some(1);
        }
    };
    let parsed = parse_args(&command.args);
    if let Some((option, _)) = parsed.unknown_flags.first() {
        let _ = writeln!(err, "Unknown option --{option} for \"{}\".", name(command.kind));
        let _ = writeln!(err, "Use \"ultron --help\" or \"{}\".", usage(command.kind));
        return Some(1);
    }
    if !parsed.diagnostics.is_empty() {
        let messages: Vec<&str> = parsed.diagnostics.iter().map(|d| d.message.as_str()).collect();
        let _ = writeln!(err, "Error: {}", messages.join("\n"));
        return Some(if command.kind == Kind::Check { 2 } else { 1 });
    }
    if command.kind != Kind::Check {
        return Some(write_print_outcome(
            print_credential(&command, &parsed, env, options),
            out,
            err,
        ));
    }
    Some(write_check_outcome(
        &command,
        check(&command, &parsed, env, options),
        out,
        err,
    ))
}
