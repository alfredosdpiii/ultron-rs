//! Kernel configuration: the environment the kernel receives, its resource limits and the output budget.
//! Ports of `kernelEnvironment`, `kernelResourceLimits` (kernel.ts) and `rlmOutputBudget` (output-truncation.ts).

use crate::KernelError;
use std::collections::HashSet;
use std::ffi::OsStr;

/// Names (ASCII case-insensitive) that mark a variable as a credential: kernel.ts `CREDENTIAL_NAME`.
const CREDENTIAL_WORDS: &[&str] = &[
    "APIKEY",
    "API_KEY",
    "ACCESSKEY",
    "ACCESS_KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "PRIVATEKEY",
    "PRIVATE_KEY",
    "AUTH",
    "COOKIE",
    "SESSIONKEY",
    "SESSION_KEY",
    "WEBHOOK",
];

/// Environment for model-written Python: credentials and the session worker's control variables are removed so the
/// kernel gets no ambient authority. `ULTRON_RLM_ENV_ALLOW` lists names (comma-separated) to pass through anyway.
pub fn kernel_environment<K: AsRef<OsStr>, V: AsRef<OsStr>>(source: impl IntoIterator<Item = (K, V)>) -> Vec<(K, V)> {
    let source: Vec<(K, V)> = source.into_iter().collect();
    let allowed: HashSet<String> = source
        .iter()
        .find(|(name, _)| name.as_ref() == crate::env::RLM_ENV_ALLOW_ENV)
        .map(|(_, value)| {
            value
                .as_ref()
                .to_string_lossy()
                .split(',')
                .map(|name| jsrt::string::trim(name).to_string())
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default();
    source
        .into_iter()
        .filter(|(name, _)| {
            let name = name.as_ref().to_string_lossy();
            let control = name.starts_with("PI_SESSION_WORKER_") || name.starts_with("ULTRON_SESSION_WORKER_");
            let upper = name.to_ascii_uppercase();
            let credential = CREDENTIAL_WORDS.iter().any(|word| upper.contains(word));
            allowed.contains(name.as_ref()) || !(control || credential)
        })
        .collect()
}

/// Per-kernel resource limits; 0 disables one. Resource limits, not isolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelResourceLimits {
    /// RLIMIT_DATA in MiB for the kernel and, inherited, every process a cell spawns.
    pub max_memory_mb: u64,
    /// CPU time per cell.
    pub max_cpu_seconds: u64,
}

pub const DEFAULT_KERNEL_LIMITS: KernelResourceLimits = KernelResourceLimits {
    max_memory_mb: 4096,
    max_cpu_seconds: 1800,
};

/// Explicit limits that take precedence over the environment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LimitOverrides {
    pub max_memory_mb: Option<u64>,
    pub max_cpu_seconds: Option<u64>,
}

/// Limits from `ULTRON_RLM_MAX_MEMORY_MB` and `ULTRON_RLM_MAX_CPU_SECONDS` (read through `lookup`), then overrides.
/// A malformed value falls back to the default; a value over 2^53 - 1 is an error, as in TypeScript.
pub fn kernel_resource_limits(
    lookup: impl Fn(&str) -> Option<String>,
    overrides: LimitOverrides,
) -> Result<KernelResourceLimits, KernelError> {
    let value = |name: &str, fallback: u64| -> Result<u64, ()> {
        let Some(raw) = lookup(name) else {
            return Ok(fallback);
        };
        let digits = jsrt::string::trim(&raw);
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Ok(fallback);
        }
        match digits.parse::<u64>() {
            Ok(number) if number <= jsrt::number::MAX_SAFE_INTEGER as u64 => Ok(number),
            _ => Err(()),
        }
    };
    let field = |name: &str, override_value: Option<u64>, env: &str, fallback: u64| {
        let parsed = match override_value {
            Some(number) if number <= jsrt::number::MAX_SAFE_INTEGER as u64 => Ok(number),
            Some(_) => Err(()),
            None => value(env, fallback),
        };
        parsed.map_err(|()| KernelError::new(format!("RLM {name} must be a non-negative integer")))
    };
    Ok(KernelResourceLimits {
        max_memory_mb: field(
            "maxMemoryMb",
            overrides.max_memory_mb,
            crate::env::RLM_MAX_MEMORY_MB_ENV,
            DEFAULT_KERNEL_LIMITS.max_memory_mb,
        )?,
        max_cpu_seconds: field(
            "maxCpuSeconds",
            overrides.max_cpu_seconds,
            crate::env::RLM_MAX_CPU_SECONDS_ENV,
            DEFAULT_KERNEL_LIMITS.max_cpu_seconds,
        )?,
    })
}

const DEFAULT_RLM_OUTPUT_BYTES: u64 = 20_000;
const MAX_RLM_OUTPUT_BYTES: u64 = 128 * 1024;

/// The byte budget for one cell's captured stream: `ULTRON_RLM_OUTPUT_BYTES` (as JavaScript's `Number` reads it),
/// else 20 KB, within 1 KB..128 KiB.
pub fn rlm_output_budget(value: Option<&str>) -> u64 {
    let raw = jsrt::string::trim(value.unwrap_or(""));
    match Some(jsrt::number::parse(raw)).filter(|n| !n.is_nan()) {
        Some(number) if number.fract() == 0.0 && number.is_finite() && number > 0.0 => {
            number.clamp(1024.0, MAX_RLM_OUTPUT_BYTES as f64) as u64
        }
        _ => DEFAULT_RLM_OUTPUT_BYTES,
    }
}
