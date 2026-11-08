//! This crate's integration tests as one test binary (see AGENTS.md).

use model_scope::{ScopeModel, parse_model_pattern};

struct M(&'static str);

impl ScopeModel for M {
    fn provider(&self) -> &str {
        "p"
    }
    fn id(&self) -> &str {
        self.0
    }
    fn name(&self) -> &str {
        self.0
    }
    fn full_reference_only(&self) -> bool {
        false
    }
}

#[test]
fn model_scope_partial_match_takes_the_highest_id_by_locale_compare() {
    // ICU puts whitespace before digits (node: "a b".localeCompare("a1") === -1), so the highest of
    // ["a b", "a1"] is "a1"; the old ASCII-class copy sorted the space after digits and picked "a b".
    let models = [M("a b"), M("a1")];
    assert_eq!(parse_model_pattern("a", &models, false).model.map(|m| m.0), Some("a1"));
    // Accents are a secondary difference: "é" sorts before "f" ("éa".localeCompare("fa") === -1).
    let models = [M("fa"), M("éa")];
    assert_eq!(parse_model_pattern("a", &models, false).model.map(|m| m.0), Some("fa"));
}

#[test]
fn model_scope_is_model_ref_matches_the_javascript_pattern() {
    for ok in ["a/b", "openrouter/x/y-1"] {
        assert!(model_scope::is_model_ref(ok), "{ok}");
    }
    for bad in ["a", "a/", "/b", "a//b", "a/b c", "a/\u{feff}b", ""] {
        assert!(!model_scope::is_model_ref(bad), "{bad:?}");
    }
}
