//! Container credentials (`fromHttp`, `fromContainerMetadata`) and EC2 instance metadata (`fromInstanceMetadata`).

use super::ini::profile_config_value;
use super::{ChainInput, CredentialError, Result, json_body, request};
use crate::aws::AwsCredentials;
use crate::http::HeaderMap;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

const METADATA_TIMEOUT: Duration = Duration::from_millis(1000);
/// The shortest static-stability extension (`STATIC_STABILITY_REFRESH_INTERVAL_SECONDS`; the SDK adds up to five
/// minutes of jitter).
const STATIC_STABILITY_EXTENSION_MS: i64 = 300_000;
const IMDS_PATH: &str = "/latest/meta-data/iam/security-credentials/";

/// `isImdsCredentials` and `fromImdsCredentials` (`expiration: new Date(Expiration)`). With `static_stability`
/// (instance metadata), credentials that already expired are extended 5 to 10 minutes (`staticStabilityProvider`),
/// so they are not refreshed during this call.
fn is_imds_credentials(input: &ChainInput<'_>, value: &Value, static_stability: bool) -> Option<AwsCredentials> {
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    let expiration = text("Expiration")?;
    let credentials = AwsCredentials {
        access_key_id: text("AccessKeyId")?,
        secret_access_key: text("SecretAccessKey")?,
        session_token: Some(text("Token")?),
    };
    let ms = jsrt::date::parse_date(&expiration);
    input
        .expiration
        .set(if static_stability && ms.is_some_and(|ms| ms < input.now_ms) {
            Some(input.now_ms + STATIC_STABILITY_EXTENSION_MS)
        } else {
            ms
        });
    Some(credentials)
}

/// `remoteProvider`: container credentials when a container URI is set, else instance metadata unless disabled.
pub(super) async fn remote_provider(input: &ChainInput<'_>) -> Result<AwsCredentials> {
    if input.var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some()
        || input.var("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some()
    {
        return container_credentials(input).await;
    }
    if input.var("AWS_EC2_METADATA_DISABLED").is_some_and(|v| v != "false") {
        return Err(CredentialError::next("EC2 Instance Metadata Service access disabled"));
    }
    from_instance_metadata(input).await
}

/// `checkUrl` of `fromHttp`.
fn http_url_accepted(url: &reqwest::Url) -> bool {
    if url.scheme() == "https" {
        return true;
    }
    let host = url.host_str().unwrap_or_default();
    if [
        "169.254.170.2",
        "169.254.170.23",
        "[fd00:ec2::23]",
        "[::1]",
        "localhost",
    ]
    .contains(&host)
    {
        return true;
    }
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4 && parts[0] == "127" && parts[1..].iter().all(|p| p.parse::<u8>().is_ok())
}

/// The setup of `fromHttp`: the container endpoint URL, checked. Upstream builds it before the provider chain runs,
/// so its failure is the whole remote link's.
fn http_target(input: &ChainInput<'_>) -> Result<reqwest::Url> {
    let host = match (
        input.var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"),
        input.var("AWS_CONTAINER_CREDENTIALS_FULL_URI"),
    ) {
        (Some(relative), _) => format!("http://169.254.170.2{relative}"),
        (None, Some(full)) => full,
        (None, None) => {
            return Err(CredentialError::next(
                "No HTTP credential provider host provided.\nSet AWS_CONTAINER_CREDENTIALS_FULL_URI or AWS_CONTAINER_CREDENTIALS_RELATIVE_URI.",
            ));
        }
    };
    let url = reqwest::Url::parse(&host).map_err(|_| CredentialError::plain("TypeError", "Invalid URL"))?;
    if !http_url_accepted(&url) {
        return Err(CredentialError::next(
            "URL not accepted. It must either be HTTPS or match one of the following:\n  - loopback CIDR 127.0.0.0/8 or [::1/128]\n  - ECS container host 169.254.170.2\n  - EKS container host 169.254.170.23 or [fd00:ec2::23]",
        ));
    }
    Ok(url)
}

/// `chain(fromHttp, fromContainerMetadata)`.
pub(super) async fn container_credentials(input: &ChainInput<'_>) -> Result<AwsCredentials> {
    let url = http_target(input)?;
    match from_http(input, &url).await {
        Ok(credentials) => Ok(credentials),
        Err(error) if error.try_next => from_container_metadata(input).await,
        Err(error) => Err(error),
    }
}

enum HttpAttemptError {
    Fatal(CredentialError),
    Retry(CredentialError),
}

async fn from_http_attempt(
    input: &ChainInput<'_>,
    url: &reqwest::Url,
    token_file: Option<&str>,
    token: Option<&str>,
) -> std::result::Result<AwsCredentials, HttpAttemptError> {
    let mut headers = HeaderMap::new();
    let authorization = match token_file {
        Some(path) => {
            let owned = path.to_string();
            let text = tokio::task::spawn_blocking(move || std::fs::read_to_string(&owned))
                .await
                .map_err(|e| HttpAttemptError::Fatal(CredentialError::plain("Error", e.to_string())))?
                .map_err(|e| {
                    HttpAttemptError::Fatal(CredentialError::plain(
                        "Error",
                        jsrt::fs::error_message(&e, "open", Path::new(path)),
                    ))
                })?;
            Some(text)
        }
        None => token.map(str::to_string),
    };
    if let Some(authorization) = authorization {
        if authorization.contains("\r\n") {
            return Err(HttpAttemptError::Retry(CredentialError::next(
                "Authorization token contains invalid \\r\\n sequence.",
            )));
        }
        headers.insert("authorization".into(), authorization);
    }
    match request(
        input,
        "GET",
        url.to_string(),
        headers,
        String::new(),
        Some(METADATA_TIMEOUT),
    )
    .await
    {
        Ok(reply) if reply.status == 200 => is_imds_credentials(input, &json_body(&reply.body), false).ok_or_else(|| {
            HttpAttemptError::Fatal(CredentialError::next(
                "HTTP credential provider response not of the required format, an object matching: { AccessKeyId: string, SecretAccessKey: string, Token: string, Expiration: string(rfc3339) }",
            ))
        }),
        Ok(reply) => Err(HttpAttemptError::Retry(CredentialError::next(format!(
            "CredentialsProviderError: Server responded with status: {}",
            reply.status
        )))),
        Err(error) => Err(HttpAttemptError::Retry(CredentialError::next(format!("Error: {error}")))),
    }
}

/// `fromHttp`: the container credentials endpoint, retried three times a second apart.
async fn from_http(input: &ChainInput<'_>, url: &reqwest::Url) -> Result<AwsCredentials> {
    let token_file = input.var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE");
    let token = input.var("AWS_CONTAINER_AUTHORIZATION_TOKEN");
    let mut last = None;
    for attempt in 0..4 {
        if attempt > 0 {
            tokio::time::sleep(METADATA_TIMEOUT).await;
        }
        match from_http_attempt(input, url, token_file.as_deref(), token.as_deref()).await {
            Ok(credentials) => return Ok(credentials),
            Err(HttpAttemptError::Fatal(error)) => return Err(error),
            Err(HttpAttemptError::Retry(error)) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| CredentialError::next("Could not load credentials from any providers")))
}

fn container_metadata_url(input: &ChainInput<'_>) -> Result<String> {
    if let Some(relative) = input.var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
        return Ok(format!("http://169.254.170.2{relative}"));
    }
    let Some(full) = input.var("AWS_CONTAINER_CREDENTIALS_FULL_URI") else {
        return Err(CredentialError::stop(
            "The container metadata credential provider cannot be used unless the AWS_CONTAINER_CREDENTIALS_RELATIVE_URI or AWS_CONTAINER_CREDENTIALS_FULL_URI environment variable is set",
        ));
    };
    let parsed = reqwest::Url::parse(&full)
        .map_err(|_| CredentialError::stop(format!("{full} is not a valid container metadata service URL")))?;
    let host = parsed.host_str().unwrap_or_default();
    if host != "localhost" && host != "127.0.0.1" {
        return Err(CredentialError::stop(format!(
            "{host} is not a valid container metadata service hostname"
        )));
    }
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(CredentialError::stop(format!(
            "{}: is not a valid container metadata service protocol",
            parsed.scheme()
        )));
    }
    Ok(parsed.to_string())
}

async fn from_container_metadata(input: &ChainInput<'_>) -> Result<AwsCredentials> {
    let url = container_metadata_url(input)?;
    let mut headers = HeaderMap::new();
    if let Some(token) = input.var("AWS_CONTAINER_AUTHORIZATION_TOKEN") {
        headers.insert("authorization".into(), token);
    }
    let body = metadata_get(input, "GET", url, headers).await?;
    is_imds_credentials(input, &json_body(&body), false)
        .ok_or_else(|| CredentialError::next("Invalid response received from instance metadata service."))
}

/// `httpRequest` of the metadata providers: a 1 s timeout, and non-2xx as a provider error.
async fn metadata_get(input: &ChainInput<'_>, method: &str, url: String, headers: HeaderMap) -> Result<String> {
    match request(input, method, url, headers, String::new(), Some(METADATA_TIMEOUT)).await {
        Ok(reply) if (200..300).contains(&reply.status) => Ok(reply.body),
        Ok(reply) => Err(CredentialError {
            name: "ProviderError".into(),
            message: "Error response received from instance metadata service".into(),
            try_next: true,
            status: Some(reply.status),
        }),
        Err(error) if error == "TimeoutError" => Err(CredentialError {
            name: "ProviderError".into(),
            message: "TimeoutError from instance metadata service".into(),
            try_next: true,
            status: None,
        }),
        Err(_) => Err(CredentialError {
            name: "ProviderError".into(),
            message: "Unable to connect to instance metadata service".into(),
            try_next: true,
            status: None,
        }),
    }
}

fn imds_base_endpoint(input: &ChainInput<'_>, profile: &str) -> Result<String> {
    let endpoint = match input
        .var("AWS_EC2_METADATA_SERVICE_ENDPOINT")
        .or_else(|| profile_config_value(profile, "ec2_metadata_service_endpoint", input.env))
    {
        Some(endpoint) => endpoint,
        None => {
            let mode = input
                .var("AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE")
                .or_else(|| profile_config_value(profile, "ec2_metadata_service_endpoint_mode", input.env))
                .unwrap_or_else(|| "IPv4".into());
            match mode.as_str() {
                "IPv4" => "http://169.254.169.254".into(),
                "IPv6" => "http://[fd00:ec2::254]".into(),
                other => {
                    return Err(CredentialError::plain(
                        "Error",
                        format!("Unsupported endpoint mode: {other}. Select from IPv4,IPv6"),
                    ));
                }
            }
        }
    };
    Ok(reqwest::Url::parse(&endpoint).map_or_else(
        |_| endpoint.trim_end_matches('/').to_string(),
        |url| url.origin().ascii_serialization(),
    ))
}

fn check_imds_v1_allowed(input: &ChainInput<'_>, profile: &str) -> Result<()> {
    let (blocked, cause) = match input.var("AWS_EC2_METADATA_V1_DISABLED") {
        Some(value) => (
            value != "false",
            "process environment variable (AWS_EC2_METADATA_V1_DISABLED)",
        ),
        None => (
            profile_config_value(profile, "ec2_metadata_v1_disabled", input.env)
                .is_some_and(|v| !v.is_empty() && v != "false"),
            "config file profile (ec2_metadata_v1_disabled)",
        ),
    };
    if blocked {
        return Err(CredentialError {
            name: "InstanceMetadataV1FallbackError".into(),
            message: format!(
                "AWS EC2 Metadata v1 fallback has been blocked by AWS SDK configuration in the following: [{cause}]."
            ),
            try_next: true,
            status: None,
        });
    }
    Ok(())
}

async fn imds_headers(input: &ChainInput<'_>, profile: &str, base: &str) -> Result<HeaderMap> {
    let mut token_headers = HeaderMap::new();
    token_headers.insert("x-aws-ec2-metadata-token-ttl-seconds".into(), "21600".into());
    let token = match metadata_get(input, "PUT", format!("{base}/latest/api/token"), token_headers).await {
        Ok(token) => Some(token),
        Err(error) if error.status == Some(400) => {
            return Err(CredentialError {
                message: "EC2 Metadata token request returned error".into(),
                ..error
            });
        }
        Err(_) => None,
    };
    let mut headers = HeaderMap::new();
    match token {
        Some(token) => {
            headers.insert("x-aws-ec2-metadata-token".into(), token);
        }
        None => check_imds_v1_allowed(input, profile)?,
    }
    Ok(headers)
}

/// `fromInstanceMetadata`: IMDSv2 (a session token first), falling back to v1 unless that is disabled.
pub(super) async fn from_instance_metadata(input: &ChainInput<'_>) -> Result<AwsCredentials> {
    let profile = input.profile_name();
    let base = imds_base_endpoint(input, &profile)?;
    let headers = imds_headers(input, &profile, &base).await?;
    let role = metadata_get(input, "GET", format!("{base}{IMDS_PATH}"), headers.clone()).await?;
    let body = metadata_get(input, "GET", format!("{base}{IMDS_PATH}{}", role.trim()), headers).await?;
    is_imds_credentials(input, &json_body(&body), true)
        .ok_or_else(|| CredentialError::next("Invalid response received from instance metadata service."))
}
