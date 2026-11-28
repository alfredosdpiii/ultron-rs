pub use crate::objects::*;
use jsonschema::{
    check, errors, grapheme_count, prepare_tool_arguments, unchecked_keyword, validate_json_schema,
    validate_tool_arguments,
};
use serde_json::Value;
use stitch::*;

endpoint! {
    fn meta_validate_schema(raw: Str<512, SchemaDoc>) {
        let Ok(schema) = serde_json::from_str::<Value>(&raw) else {
            return;
        };
        let _ = validate_json_schema(&schema, "$");
        let _ = unchecked_keyword(&schema, "$");
    }
}

endpoint! {
    fn check_and_errors(raw_schema: Str<512, SchemaDoc>, raw_value: Str<512, JsonDoc>) {
        let Ok(schema) = serde_json::from_str::<Value>(&raw_schema) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw_value) else {
            return;
        };
        let ok = check(&schema, &value);
        let errs = errors(&schema, &value);
        if ok && !errs.is_empty() {
            panic!("jsonschema invariant violated: check=true but errors non-empty");
        }
    }
}

endpoint! {
    fn tool_argument_validation(raw_schema: Str<512, SchemaDoc>, raw_args: Str<512, JsonDoc>) {
        let Ok(schema) = serde_json::from_str::<Value>(&raw_schema) else {
            return;
        };
        let Ok(args) = serde_json::from_str::<Value>(&raw_args) else {
            return;
        };
        let _ = validate_tool_arguments("fuzz_tool", &schema, &args);
        if let Some(map) = args.as_object() {
            let _ = prepare_tool_arguments("fuzz_tool", &schema, map);
        }
    }
}

endpoint! {
    fn grapheme_and_format_checks(raw: Str<512, JsonDoc>) {
        let _ = grapheme_count(&raw);
    }
}
