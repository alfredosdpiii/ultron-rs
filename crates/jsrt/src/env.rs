//! Shared environment-flag parsers (`"off"`, `"0"`, `"false"`, `"no"`), trimmed and ASCII case-insensitive.

/// Whether an optional environment variable turns a feature off (`"off"`, `"0"`, or `"false"`).
pub fn flag_off(value: Option<&str>) -> bool {
    value.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false"))
}

/// Whether an optional environment variable turns a feature off (`"off"`, `"0"`, `"false"`, or `"no"`).
pub fn flag_off_or_no(value: Option<&str>) -> bool {
    value.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_off_matches_trimmed_case_insensitive_off_values() {
        for off in ["off", "OFF", " 0 ", "False", "false\n"] {
            assert!(flag_off(Some(off)), "{off}");
            assert!(flag_off_or_no(Some(off)), "{off}");
        }
        assert!(!flag_off(Some("no")));
        assert!(flag_off_or_no(Some(" NO ")));
        for on in ["", "1", "true", "on"] {
            assert!(!flag_off(Some(on)), "{on}");
            assert!(!flag_off_or_no(Some(on)), "{on}");
        }
        assert!(!flag_off(None));
    }
}
