//! JavaScript coercions of a JSON value: truthiness, `String(value)` and `Number(value)`.
//! `None` stands for `undefined` (a missing key).

use serde_json::Value;

/// JavaScript truthiness of a JSON value; `None` is `undefined`. `0`, `-0`, `""`, `null` and `false` are falsy;
/// `"0"`, `"false"`, `[]` and `{}` are truthy. (A `serde_json::Number` is never `NaN`.)
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// `String(value)`; `None` is `undefined`. Arrays join their items with `,`, `null` items as empty strings; objects
/// are `[object Object]`; numbers print as `Number.prototype.toString` does (`1.0` is `1`, `1e21` is `1e+21`).
pub fn string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => crate::number::json_to_string(n),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                if item.is_null() {
                    String::new()
                } else {
                    string(Some(item))
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// `Number(value)`: `null` is 0, booleans 0/1, strings as [`crate::number::parse`], arrays and objects `NaN`.
/// (`Number(undefined)` is `NaN`: a caller with a missing key checks for it first.)
// ponytail: `Number([5])` is 5 in JavaScript (via `String([5])`); no caller coerces arrays.
pub fn number(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => crate::number::json_value(n),
        Value::String(s) => crate::number::parse(s),
        Value::Array(_) | Value::Object(_) => f64::NAN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn jsrt_value_truthy_follows_javascript() {
        for falsy in [json!(null), json!(false), json!(0), json!(-0.0), json!(0.0), json!("")] {
            assert!(!truthy(Some(&falsy)), "{falsy} is falsy");
        }
        for value in [
            json!(true),
            json!(1),
            json!(-1),
            json!(0.5),
            json!("0"),
            json!("false"),
            json!(" "),
        ] {
            assert!(truthy(Some(&value)), "{value} is truthy");
        }
        assert!(truthy(Some(&json!([]))) && truthy(Some(&json!({}))));
        assert!(!truthy(None));
        // `JSON.parse("-0")` is -0, which is falsy.
        assert!(!truthy(Some(&serde_json::from_str::<Value>("-0").unwrap())));
    }

    #[test]
    fn jsrt_value_string_is_javascripts_string() {
        // From node: String([1, null, [2, undefined], {}]), String(null), String([null]), String([[null, 1]]),
        // String(1e21), String(-0).
        assert_eq!(string(Some(&json!([1, null, [2, null], {}]))), "1,,2,,[object Object]");
        assert_eq!(string(Some(&json!(null))), "null");
        assert_eq!(string(Some(&json!([null]))), "");
        assert_eq!(string(Some(&json!([[null, 1]]))), ",1");
        assert_eq!(string(Some(&json!(1e21))), "1e+21");
        assert_eq!(string(Some(&json!(-0.0))), "0");
        assert_eq!(string(Some(&json!(1.0))), "1");
        assert_eq!(string(Some(&json!(true))), "true");
        assert_eq!(string(None), "undefined");
    }

    #[test]
    fn jsrt_value_number_coerces_like_number() {
        assert_eq!(number(&Value::Null), 0.0);
        assert_eq!(number(&Value::Bool(true)), 1.0);
        assert_eq!(number(&Value::from(" 0b11 ")), 3.0);
        assert_eq!(number(&Value::from("")), 0.0);
        assert!(number(&Value::from("5s")).is_nan());
        assert!(number(&json!({})).is_nan());
        assert!(number(&json!([1])).is_nan());
        let big: Value = serde_json::from_str("9007199254740993").unwrap();
        assert_eq!(number(&big), 9_007_199_254_740_992.0);
    }
}
