//! STS Query calls (`AssumeRole`, `AssumeRoleWithWebIdentity`) and web identity token-file credentials.

use super::{ChainInput, CredentialError, Reply, Result, configured_endpoint, regional_endpoint, request};
use crate::aws::{AwsCredentials, SignableRequest, escape_uri, sign_request};
use crate::http::HeaderMap;
use std::path::Path;

/// The STS modeled errors (`awsQueryError` codes to exception names).
fn sts_error_name(code: &str) -> String {
    match code {
        "IDPCommunicationError" => "IDPCommunicationErrorException".into(),
        "IDPRejectedClaim" => "IDPRejectedClaimException".into(),
        "InvalidIdentityToken" => "InvalidIdentityTokenException".into(),
        "MalformedPolicyDocument" => "MalformedPolicyDocumentException".into(),
        "PackedPolicyTooLarge" => "PackedPolicyTooLargeException".into(),
        other => other.into(),
    }
}

fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The text of the first `<tag>` element.
fn xml_tag(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&format!("</{tag}>"))? + start;
    Some(xml_unescape(&text[start..end]))
}

fn sign_sts_headers(
    input: &ChainInput<'_>,
    url: &str,
    body: &str,
    region: &str,
    credentials: &AwsCredentials,
) -> Result<HeaderMap> {
    let parsed = reqwest::Url::parse(url).map_err(|e| CredentialError::plain("TypeError", e.to_string()))?;
    let host = match (parsed.host_str(), parsed.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => String::new(),
    };
    let mut signable = SignableRequest {
        method: "POST".into(),
        path: parsed.path().to_string(),
        query: Vec::new(),
        headers: vec![
            ("content-type".into(), "application/x-www-form-urlencoded".into()),
            ("host".into(), host),
        ],
        body: body.as_bytes().to_vec(),
    };
    sign_request(&mut signable, credentials, region, "sts", input.now_ms);
    Ok(signable
        .headers
        .into_iter()
        .filter(|(name, _)| name != "host")
        .collect())
}

fn parse_sts_response(input: &ChainInput<'_>, reply: &Reply, role_arn: &str, label: &str) -> Result<AwsCredentials> {
    if !(200..300).contains(&reply.status) {
        let code = xml_tag(&reply.body, "Code").unwrap_or_else(|| "UnknownError".into());
        return Err(CredentialError {
            name: sts_error_name(&code),
            message: xml_tag(&reply.body, "Message").unwrap_or_else(|| "UnknownError".into()),
            try_next: false,
            status: Some(reply.status),
        });
    }
    let credentials = xml_tag(&reply.body, "Credentials").unwrap_or_default();
    match (
        xml_tag(&credentials, "AccessKeyId").filter(|s| !s.is_empty()),
        xml_tag(&credentials, "SecretAccessKey").filter(|s| !s.is_empty()),
    ) {
        (Some(access_key_id), Some(secret_access_key)) => {
            input.expiration.set(
                xml_tag(&credentials, "Expiration")
                    .as_deref()
                    .and_then(jsrt::date::parse_date),
            );
            Ok(AwsCredentials {
                access_key_id,
                secret_access_key,
                session_token: xml_tag(&credentials, "SessionToken"),
            })
        }
        _ => Err(CredentialError::plain(
            "Error",
            format!("Invalid response from STS.{label} call with role {role_arn}"),
        )),
    }
}

/// One STS Query call returning `Credentials`; unsigned without `sign`.
pub(super) async fn sts_call(
    input: &ChainInput<'_>,
    region: &str,
    action: &str,
    params: &[(&str, String)],
    sign: Option<&AwsCredentials>,
    role_arn: &str,
    label: &str,
) -> Result<AwsCredentials> {
    let endpoint = configured_endpoint(input, "STS").unwrap_or_else(|| regional_endpoint("sts", region));
    let mut body = format!("Action={action}&Version=2011-06-15");
    for (key, value) in params {
        body.push_str(&format!("&{key}={}", escape_uri(value)));
    }
    let url = format!("{endpoint}/");
    let headers = match sign {
        Some(credentials) => sign_sts_headers(input, &url, &body, region, credentials)?,
        None => {
            let mut headers = HeaderMap::new();
            headers.insert("content-type".into(), "application/x-www-form-urlencoded".into());
            headers
        }
    };
    let reply = request(input, "POST", url, headers, body, None)
        .await
        .map_err(|e| CredentialError::plain("Error", e))?;
    parse_sts_response(input, &reply, role_arn, label)
}

pub(super) struct WebIdentity {
    pub(super) token_file: Option<String>,
    pub(super) role_arn: Option<String>,
    pub(super) session_name: Option<String>,
}

/// `fromTokenFile`: the profile's settings, else `AWS_WEB_IDENTITY_TOKEN_FILE`/`AWS_ROLE_ARN`/`AWS_ROLE_SESSION_NAME`.
pub(super) async fn from_token_file(input: &ChainInput<'_>, config: Option<WebIdentity>) -> Result<AwsCredentials> {
    let config = config.unwrap_or(WebIdentity {
        token_file: None,
        role_arn: None,
        session_name: None,
    });
    let token_file = config.token_file.or_else(|| input.var("AWS_WEB_IDENTITY_TOKEN_FILE"));
    let role_arn = config.role_arn.or_else(|| input.var("AWS_ROLE_ARN"));
    let session_name = config.session_name.or_else(|| input.var("AWS_ROLE_SESSION_NAME"));
    let (Some(token_file), Some(role_arn)) = (token_file.filter(|f| !f.is_empty()), role_arn.filter(|r| !r.is_empty()))
    else {
        return Err(CredentialError::next("Web identity configuration not specified"));
    };
    let owned_file = token_file.clone();
    let token = tokio::task::spawn_blocking(move || std::fs::read(&owned_file))
        .await
        .map_err(|e| CredentialError::plain("Error", e.to_string()))?
        .map_err(|e| CredentialError::plain("Error", jsrt::fs::error_message(&e, "open", Path::new(&token_file))))?
        .into_iter()
        .map(|b| (b & 0x7f) as char)
        .collect::<String>();
    let params = [
        ("RoleArn", role_arn.clone()),
        (
            "RoleSessionName",
            session_name.unwrap_or_else(|| format!("aws-sdk-js-session-{}", input.now_ms)),
        ),
        ("WebIdentityToken", token),
    ];
    sts_call(
        input,
        input.region,
        "AssumeRoleWithWebIdentity",
        &params,
        None,
        &role_arn,
        "assumeRoleWithWebIdentity",
    )
    .await
}
