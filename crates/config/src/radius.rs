//! The Radius gateway (providers/radius-config.ts): its default URL, the variable that overrides it, and the URL
//! normalization the provider and the relay both apply.

/// `DEFAULT_RADIUS_GATEWAY`.
pub const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";
/// `ENV_RADIUS_GATEWAY`.
pub const ENV_RADIUS_GATEWAY: &str = "PI_RADIUS_GATEWAY";

/// `normalizeRadiusGatewayUrl`: `https://` unless a scheme is given, no trailing slashes.
pub fn normalize_radius_gateway_url(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    let with_scheme = if lower.starts_with("http://") || lower.starts_with("https://") {
        value.to_string()
    } else {
        format!("https://{value}")
    };
    with_scheme.trim_end_matches('/').to_string()
}
