//! JavaScript semantics only the session format uses: `Array.prototype.join`'s `String`, `?? `/`== null`,
//! `new Date(value)` of a JSON value, and V8's comparator sort. The shared primitives are in [`jsrt`].

use serde_json::Value;

/// `Number.prototype.toString()` for a double; non-finite values become `null` as in JSON.
pub fn format_f64(f: f64) -> String {
    if f.is_finite() {
        jsrt::number::to_string(f)
    } else {
        "null".to_string()
    }
}

/// `undefined` or `null`, the values `??` and `== null` treat as missing.
pub fn nullish(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null))
}

/// `String(value)` as `Array.prototype.join` applies it: `null` and `undefined` become "".
pub fn join_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => jsrt::number::json_to_string(n),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items.iter().map(|v| join_string(Some(v))).collect::<Vec<_>>().join(","),
        Some(Value::Object(_)) => "[object Object]".to_string(),
    }
}

/// `new Date(value).getTime()` for a JSON value; `None` is `NaN` (an invalid date).
pub fn date_value(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::String(s) => jsrt::date::parse_date(s),
        Value::Array(_) | Value::Object(_) => None,
        other => jsrt::date::time_clip(jsrt::value::number(other)),
    }
}

/// A timestamp as a JSON number, or `null` for an invalid date (as `JSON.stringify(NaN)` writes it).
pub fn date_json(value: Option<&Value>) -> Value {
    date_value(value).map_or(Value::Null, Value::from)
}

/// `Array.prototype.sort` with a comparator, as V8 runs it on arrays under 64 elements (one natural
/// run, then binary insertion). A `NaN` comparison counts as equal. Stable; never panics on an
/// inconsistent comparator.
// ponytail: V8 merges runs for 64+ elements; for consistent comparators any stable sort agrees.
pub fn sort_by<T>(items: &mut [T], compare: impl Fn(&T, &T) -> f64) {
    let less = |a: &T, b: &T| compare(a, b) < 0.0;
    let n = items.len();
    if n < 2 {
        return;
    }
    let mut run = 2;
    let descending = less(&items[1], &items[0]);
    while run < n {
        let order_less = less(&items[run], &items[run - 1]);
        let equal_or_greater = !order_less;
        if (descending && equal_or_greater) || (!descending && order_less) {
            break;
        }
        run += 1;
    }
    if descending {
        items[..run].reverse();
    }
    for start in run..n {
        let (mut left, mut right) = (0, start);
        while left < right {
            let mid = left + (right - left) / 2;
            if less(&items[start], &items[mid]) {
                right = mid;
            } else {
                left = mid + 1;
            }
        }
        items[left..=start].rotate_right(1);
    }
}
