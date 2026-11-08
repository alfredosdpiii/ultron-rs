//! JavaScript numbers: `String(x)` / `Number.prototype.toString()`, `toFixed`, `Math.round`, `Number(text)`, and
//! safe integers.

use serde_json::{Number, Value};

/// `Number.MAX_SAFE_INTEGER`, 2^53 - 1.
pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// `Number.isSafeInteger(n)`: finite, integral, and within ±(2^53 - 1). `-0` is a safe integer.
pub fn is_safe_integer(n: f64) -> bool {
    n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER as f64
}

/// The JSON value as an integer when `Number.isSafeInteger(value)` holds; strings and other types are not numbers.
/// Integers parsed beyond 2^53 - 1 are refused (as `u64` or `i64` they are exact in Rust, but not in JavaScript).
pub fn safe_integer(value: &Value) -> Option<i64> {
    let n = value.as_number()?;
    if let Some(i) = n.as_i64() {
        return (i.unsigned_abs() <= MAX_SAFE_INTEGER as u64).then_some(i);
    }
    if n.is_u64() {
        return None;
    }
    let f = n.as_f64()?;
    // ponytail: `-0.0 as i64` is 0, which is what JavaScript prints for -0 anyway.
    is_safe_integer(f).then_some(f as i64)
}

/// `String(x)`: `Number.prototype.toString()` with the shortest round-tripping digits, positional notation for
/// 1e-7 < |x| < 1e21 and `1e+21` / `1e-7` exponent notation outside it. `-0` is `"0"`.
pub fn to_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if x == 0.0 {
        return "0".into();
    }
    let sign = if x < 0.0 { "-" } else { "" };
    // Rust's `{:e}` prints the shortest round-tripping digits, as ECMAScript's Number::toString chooses them.
    let sci = format!("{:e}", x.abs());
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((&sci, "0"));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i64;
    let n = exponent.parse::<i64>().unwrap_or(0) + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let e_sign = if e < 0 { '-' } else { '+' };
        if k == 1 {
            format!("{digits}e{e_sign}{}", e.abs())
        } else {
            format!("{}.{}e{e_sign}{}", &digits[..1], &digits[1..], e.abs())
        }
    };
    format!("{sign}{body}")
}

/// The number `JSON.parse` would hold for a parsed JSON number: integers beyond 2^53 round to the nearest double.
pub(crate) fn json_value(n: &Number) -> f64 {
    if let Some(i) = n.as_i64() {
        return i as f64;
    }
    if let Some(u) = n.as_u64() {
        return u as f64;
    }
    n.as_f64().unwrap_or(f64::NAN)
}

/// `String(x)` of a parsed JSON number (see [`json_value`]): `9007199254740993` prints as `9007199254740992`.
pub fn json_to_string(n: &Number) -> String {
    const SAFE: u64 = 1 << 53;
    if let Some(u) = n.as_u64() {
        if u <= SAFE {
            return u.to_string();
        }
    } else if let Some(i) = n.as_i64()
        && i.unsigned_abs() <= SAFE
    {
        return i.to_string();
    }
    to_string(json_value(n))
}

/// `x.toFixed(digits)`: the exact decimal value rounded to `digits` places, an exact tie rounding away from zero
/// (Rust's `{:.N}` rounds ties to even). `NaN`, the infinities and |x| >= 1e21 print as `String(x)`; `-0` prints
/// unsigned and a negative that rounds to zero keeps its sign (`(-0.001).toFixed(2)` is `-0.00`).
pub fn to_fixed(x: f64, digits: usize) -> String {
    if !x.is_finite() || x.abs() >= 1e21 {
        return to_string(x);
    }
    // A double has at most 1074 fractional binary digits, so 1100 decimal places are its exact expansion.
    let exact = format!("{:.1100}", x.abs());
    let (int, frac) = exact.split_once('.').unwrap_or((&exact, ""));
    let mut kept: Vec<u8> = format!("{int}{}", &frac[..digits]).into_bytes();
    if frac.as_bytes()[digits] >= b'5' {
        let mut i = kept.len();
        loop {
            if i == 0 {
                kept.insert(0, b'1');
                break;
            }
            i -= 1;
            if kept[i] == b'9' {
                kept[i] = b'0';
            } else {
                kept[i] += 1;
                break;
            }
        }
    }
    let text = String::from_utf8(kept).unwrap_or_default();
    let (whole, fraction) = text.split_at(text.len() - digits);
    let sign = if x < 0.0 { "-" } else { "" };
    if digits == 0 {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{fraction}")
    }
}

/// `Math.round(x)`: the nearest integer, halves toward +Infinity (`-2.5` is `-2`, `0.49999999999999994` is `0`).
/// Unlike `(x + 0.5).floor()`, exact for every double.
pub fn round(x: f64) -> f64 {
    let floor = x.floor();
    if x - floor >= 0.5 { floor + 1.0 } else { floor }
}

/// `Number(text)`: JavaScript whitespace trimmed; blank is 0; a decimal literal (sign, fraction, exponent), a signed
/// `Infinity`, or an unsigned `0x` / `0o` / `0b` literal; anything else (`inf`, `nan`, `1_000`, `-0x10`) is `NaN`.
pub fn parse(text: &str) -> f64 {
    let t = crate::string::trim(text);
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(rest) = t.strip_prefix(prefix) {
            if rest.is_empty() || !rest.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            // Exact while it fits in a u128 (then one correctly rounded conversion), as JavaScript reads it.
            return match u128::from_str_radix(rest, radix) {
                Ok(n) => n as f64,
                Err(_) => rest.chars().fold(0.0, |acc, c| {
                    acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
                }),
            };
        }
    }
    let unsigned = t.strip_prefix(['+', '-']).unwrap_or(t);
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(i) => (&unsigned[..i], Some(&unsigned[i + 1..])),
        None => (unsigned, None),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = digits(int) && digits(frac) && !(int.is_empty() && frac.is_empty());
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        !e.is_empty() && digits(e)
    });
    if mantissa_ok && exponent_ok {
        t.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Literals printed by node 26.

    #[test]
    fn jsrt_number_to_string_matches_number_prototype_to_string() {
        let cases: [(f64, &str); 20] = [
            (0.0, "0"),
            (-0.0, "0"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (1e-7, "1e-7"),
            (-1e-7, "-1e-7"),
            (1.5e-7, "1.5e-7"),
            (0.000001, "0.000001"),
            (123e-20, "1.23e-18"),
            (1.5e300, "1.5e+300"),
            (5e-324, "5e-324"),
            (0.1 + 0.2, "0.30000000000000004"),
            (2f64.powi(60), "1152921504606847000"),
            (123456789012345680000.0, "123456789012345680000"),
            (-1.5, "-1.5"),
            (100.0, "100"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ];
        for (x, expected) in cases {
            assert_eq!(to_string(x), expected, "{x:e}");
        }
    }

    #[test]
    fn jsrt_number_json_integers_round_like_json_parse() {
        let parsed: Vec<Number> =
            serde_json::from_str("[9007199254740993, 12345678901234567890, -9007199254740993, 9007199254740992]")
                .unwrap();
        let printed: Vec<String> = parsed.iter().map(json_to_string).collect();
        assert_eq!(
            printed,
            [
                "9007199254740992",
                "12345678901234567000",
                "-9007199254740992",
                "9007199254740992"
            ]
        );
    }

    #[test]
    fn jsrt_number_to_fixed_rounds_the_exact_value_with_ties_away_from_zero() {
        // From node: (2.25).toFixed(1), (0.125).toFixed(2), (1.005).toFixed(2), (9.95).toFixed(1), ...
        assert_eq!(to_fixed(2.25, 1), "2.3");
        assert_eq!(to_fixed(0.125, 2), "0.13");
        assert_eq!(to_fixed(1.005, 2), "1.00");
        assert_eq!(to_fixed(9.95, 1), "9.9");
        assert_eq!(to_fixed(0.31, 2), "0.31");
        assert_eq!(to_fixed(9.995, 2), "9.99");
        assert_eq!(to_fixed(0.5, 0), "1");
        assert_eq!(to_fixed(9.5, 0), "10");
        assert_eq!(to_fixed(2.5, 0), "3");
        assert_eq!(to_fixed(-1.5, 0), "-2");
        assert_eq!(to_fixed(-2.25, 1), "-2.3");
        assert_eq!(to_fixed(1.15, 1), "1.1");
        assert_eq!(to_fixed(1.45, 1), "1.4");
        assert_eq!(to_fixed(1.255, 2), "1.25");
        assert_eq!(to_fixed(1280.0 / 1024.0, 1), "1.3");
        assert_eq!(to_fixed(1.15, 1), "1.1");
        assert_eq!(to_fixed(123.456, 0), "123");
        assert_eq!(to_fixed(0.0, 2), "0.00");
        assert_eq!(to_fixed(1e-10, 3), "0.000");
        // Sign, non-finite and huge values.
        assert_eq!(to_fixed(-0.0, 1), "0.0");
        assert_eq!(to_fixed(-0.001, 1), "-0.0");
        assert_eq!(to_fixed(f64::NAN, 2), "NaN");
        assert_eq!(to_fixed(f64::INFINITY, 2), "Infinity");
        assert_eq!(to_fixed(f64::NEG_INFINITY, 1), "-Infinity");
        assert_eq!(to_fixed(1e21, 2), "1e+21");
    }

    #[test]
    fn jsrt_number_round_is_math_round() {
        assert_eq!(round(0.5), 1.0);
        assert_eq!(round(2.5), 3.0);
        assert_eq!(round(-2.5), -2.0);
        assert_eq!(round(-0.5), 0.0);
        assert_eq!(round(0.49999999999999994), 0.0);
        assert_eq!(round(4_503_599_627_370_497.0), 4_503_599_627_370_497.0);
        assert!(round(f64::NAN).is_nan());
        assert_eq!(round(f64::INFINITY), f64::INFINITY);
    }

    #[test]
    fn jsrt_number_parse_is_number_of_a_string() {
        let cases: [(&str, Option<f64>); 26] = [
            ("", Some(0.0)),
            (" 12 ", Some(12.0)),
            ("0x1F", Some(31.0)),
            ("0o17", Some(15.0)),
            ("0b101", Some(5.0)),
            ("-0x10", None),
            ("+0x10", None),
            ("0x", None),
            ("1_000", None),
            ("inf", None),
            ("INF", None),
            ("nan", None),
            ("Infinity", Some(f64::INFINITY)),
            ("+Infinity", Some(f64::INFINITY)),
            ("-Infinity", Some(f64::NEG_INFINITY)),
            (".5", Some(0.5)),
            ("5.", Some(5.0)),
            ("+5", Some(5.0)),
            ("1e", None),
            ("1e+3", Some(1000.0)),
            (" \u{a0}\u{feff}7\u{2028}", Some(7.0)),
            ("\u{85}7", None),
            ("1.2.3", None),
            ("e5", None),
            ("+-1", None),
            ("0x20000000000001", Some(9_007_199_254_740_992.0)),
        ];
        for (text, expected) in cases {
            let got = parse(text);
            assert_eq!((!got.is_nan()).then_some(got), expected, "{text:?}");
        }
    }

    #[test]
    fn jsrt_safe_integer_is_number_is_safe_integer() {
        assert_eq!(safe_integer(&json!(9_007_199_254_740_991u64)), Some(MAX_SAFE_INTEGER));
        assert_eq!(safe_integer(&json!(-9_007_199_254_740_991i64)), Some(-MAX_SAFE_INTEGER));
        assert_eq!(safe_integer(&json!(9_007_199_254_740_992u64)), None);
        assert_eq!(safe_integer(&json!(9_007_199_254_740_992.0)), None);
        assert_eq!(safe_integer(&json!(u64::MAX)), None);
        assert_eq!(safe_integer(&json!(i64::MIN)), None);
        assert_eq!(safe_integer(&json!(1.0)), Some(1));
        assert_eq!(safe_integer(&json!(-0.0)), Some(0));
        assert_eq!(safe_integer(&json!(1.5)), None);
        assert_eq!(safe_integer(&json!("1")), None);
        assert_eq!(safe_integer(&json!(true)), None);
        assert_eq!(safe_integer(&json!(null)), None);
    }

    #[test]
    fn jsrt_is_safe_integer_rejects_non_finite() {
        assert!(is_safe_integer(0.0) && is_safe_integer(-0.0) && is_safe_integer(9_007_199_254_740_991.0));
        assert!(!is_safe_integer(9_007_199_254_740_992.0));
        assert!(!is_safe_integer(f64::NAN) && !is_safe_integer(f64::INFINITY) && !is_safe_integer(f64::NEG_INFINITY));
        assert!(!is_safe_integer(0.1));
    }
}
