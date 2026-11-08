//! `Date` as the ported code uses it: `Date.now()`, TimeClip, `Date.parse` for the ISO-8601 forms V8 accepts,
//! `toISOString`, and the proleptic Gregorian day arithmetic under them (Howard Hinnant's `days_from_civil` and
//! `civil_from_days`). Times are milliseconds since the epoch; `None` is an invalid date (`NaN`).
//!
//! Local time (`getHours` and friends under `TZ`) is [`crate::localtime`].

use std::time::{SystemTime, UNIX_EPOCH};

use crate::string::trim;

/// The largest time a `Date` holds, in either direction (ECMAScript 21.4.1.1): 100,000,000 days.
const MAX_TIME_MS: f64 = 8.64e15;

const DAY_MS: i64 = 86_400_000;

/// `Date.now()`: whole milliseconds since the epoch; 0 when the system clock is before the epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

/// Node's `stats.mtimeMs` (and `atimeMs`, `ctimeMs`) of a file time: `tv_sec * 1e3 + tv_nsec / 1e6`, where a time
/// before the epoch has a negative `tv_sec` and a `tv_nsec` in 0..1e9.
pub fn mtime_ms(time: SystemTime) -> f64 {
    let (secs, nanos) = match time.duration_since(UNIX_EPOCH) {
        Ok(after) => (after.as_secs() as f64, after.subsec_nanos()),
        Err(before) => {
            let before = before.duration();
            match before.subsec_nanos() {
                0 => (-(before.as_secs() as f64), 0),
                nanos => (-(before.as_secs() as f64) - 1.0, 1_000_000_000 - nanos),
            }
        }
    };
    secs * 1e3 + f64::from(nanos) / 1e6
}

/// ECMAScript TimeClip: the time a `Date` built from `ms` holds (truncated toward zero), `None` for `NaN`,
/// infinities and anything beyond ±8.64e15 ms (100,000,000 days).
pub fn time_clip(ms: f64) -> Option<i64> {
    if !ms.is_finite() || ms.abs() > MAX_TIME_MS {
        return None;
    }
    Some(ms.trunc() as i64)
}

/// Days since 1970-01-01 of a proleptic Gregorian date. `day` may run past the month's end (`2019-02-30` is
/// March 2nd), as `Date.UTC` lets it.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let (month, day) = (i64::from(month), i64::from(day));
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The proleptic Gregorian `(year, month 1-12, day 1-31)` of a day count since 1970-01-01.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

/// `new Date(ms).toISOString()`: `YYYY-MM-DDTHH:mm:ss.sssZ`, with the expanded `±YYYYYY` year outside 0000-9999.
/// (JavaScript throws for an invalid date; a caller holding an `f64` goes through [`time_clip`] first.)
pub fn iso_string(ms: i64) -> String {
    let (year, month, day) = civil_from_days(ms.div_euclid(DAY_MS));
    let rest = ms.rem_euclid(DAY_MS);
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!("{}{:06}", if year < 0 { '-' } else { '+' }, year.abs())
    };
    format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    )
}

struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    /// `min` to `max` ASCII digits as a number.
    fn digits(&mut self, min: usize, max: usize) -> Option<i64> {
        let start = self.i;
        while self.i < self.s.len() && self.i - start < max && self.s[self.i].is_ascii_digit() {
            self.i += 1;
        }
        if self.i - start < min {
            return None;
        }
        std::str::from_utf8(&self.s[start..self.i]).ok()?.parse().ok()
    }

    fn eat(&mut self, c: u8) -> bool {
        let hit = self.s.get(self.i) == Some(&c);
        self.i += usize::from(hit);
        hit
    }

    fn peek_any(&self, set: &[u8]) -> Option<u8> {
        self.s.get(self.i).copied().filter(|c| set.contains(c))
    }
}

/// `Date.parse` for the ISO-8601 forms V8 accepts: `YYYY[-MM[-DD]][THH:mm[:ss[.fff…]]][Z|±HH:mm|±HHmm]`, `±YYYYYY`
/// years, a space or lowercase `t`/`z`, one-digit month and day, a day past the month's end up to 31, `24:00`, and
/// JavaScript whitespace around the text. A date-time without an offset is read as UTC (V8 reads it as local time;
/// every caller and fixture runs in UTC). Other formats, RFC 2822 dates among them, are `None` (`NaN`).
///
/// Known gaps against V8's fallback parser, which no caller writes: `2026-10-07Z` is `None` (V8 accepts it), and
/// whitespace around a date-time (`" 2026-10-07T00:00Z"`) is accepted (V8 rejects it).
pub fn parse_date(input: &str) -> Option<i64> {
    let mut c = Cursor {
        s: trim(input).as_bytes(),
        i: 0,
    };
    let year = match c.peek_any(b"+-") {
        Some(sign) => {
            c.i += 1;
            let y = c.digits(6, 6)?;
            if sign == b'-' && y == 0 {
                return None;
            }
            if sign == b'-' { -y } else { y }
        }
        None => c.digits(4, 4)?,
    };
    let (mut month, mut day) = (1, 1);
    if c.eat(b'-') {
        month = c.digits(1, 2)?;
        if c.eat(b'-') {
            day = c.digits(1, 2)?;
        }
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut time_ms = 0;
    let mut offset_min = 0;
    if c.peek_any(b"Tt ").is_some() {
        c.i += 1;
        let hour = c.digits(2, 2)?;
        if !c.eat(b':') {
            return None;
        }
        let minute = c.digits(2, 2)?;
        let (mut second, mut millis) = (0, 0);
        if c.eat(b':') {
            second = c.digits(2, 2)?;
            if c.eat(b'.') {
                let start = c.i;
                c.digits(1, usize::MAX)?;
                let fraction = std::str::from_utf8(&c.s[start..c.i]).ok()?;
                millis = format!("{fraction:0<3}")[..3].parse::<i64>().ok()?;
            }
        }
        if hour > 24 || minute > 59 || second > 59 || (hour == 24 && (minute, second, millis) != (0, 0, 0)) {
            return None;
        }
        time_ms = ((hour * 60 + minute) * 60 + second) * 1000 + millis;
        if c.peek_any(b"Zz").is_some() {
            c.i += 1;
        } else if let Some(sign) = c.peek_any(b"+-") {
            c.i += 1;
            let offset_hours = c.digits(2, 2)?;
            c.eat(b':');
            let offset_minutes = c.digits(2, 2)?;
            if offset_hours > 23 || offset_minutes > 59 {
                return None;
            }
            offset_min = (offset_hours * 60 + offset_minutes) * if sign == b'+' { 1 } else { -1 };
        }
    }
    if c.i != c.s.len() {
        return None;
    }
    // Month and day were range-checked above, so the casts are lossless; the day may overflow the month.
    let days = days_from_civil(year, month as u32, 1) + (day - 1);
    time_clip((days * DAY_MS + time_ms - offset_min * 60_000) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from session-file/tests/upstream.rs `session_file_dates_parse_and_print_like_v8` (checked against V8).
    #[test]
    fn jsrt_date_parse_accepts_the_iso_forms_v8_does() {
        let cases: [(&str, Option<i64>); 18] = [
            ("2019-02-30", Some(1_551_484_800_000)),
            ("2019-02-31T00:00:00Z", Some(1_551_571_200_000)),
            ("2025-01-01T00:00:00.123456Z", Some(1_735_689_600_123)),
            ("2025-01-01T24:00:00Z", Some(1_735_776_000_000)),
            ("2025-01-01T00:00Z", Some(1_735_689_600_000)),
            ("2025-1-01", Some(1_735_689_600_000)),
            ("+002025-01-01T00:00:00Z", Some(1_735_689_600_000)),
            ("2025", Some(1_735_689_600_000)),
            ("2025-06", Some(1_748_736_000_000)),
            ("2025-01-01T00:00:00", Some(1_735_689_600_000)),
            ("2025-01-01T00:00:00.5+01:30", Some(1_735_684_200_500)),
            ("2025-01-01T00:00:00z", Some(1_735_689_600_000)),
            ("2025-01-01 00:00:00Z", Some(1_735_689_600_000)),
            ("2025-01-01T00:00:00+0100", Some(1_735_686_000_000)),
            (" 2025-01-01", Some(1_735_689_600_000)),
            ("2025-01-01T00:00:60Z", None),
            ("-000001-01-01T00:00:00Z", Some(-62_198_755_200_000)),
            ("2025-01-01T00:00:00.Z", None),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_date(input), expected, "{input}");
        }
    }

    // Ported from review/tests/ledger.rs `parse_iso_ms` cases.
    #[test]
    fn jsrt_date_parse_reads_github_and_ledger_times() {
        assert_eq!(parse_date("2026-10-10T00:00:00.000Z"), Some(1_791_590_400_000));
        assert_eq!(parse_date("2026-09-20"), Some(1_789_862_400_000));
        assert_eq!(parse_date("2026-09-10T00:00:00+02:00"), Some(1_788_991_200_000));
        assert_eq!(parse_date("2026-08-01T12:30:45.5Z"), Some(1_785_587_445_500));
        assert_eq!(parse_date("1969-12-31T23:59:59Z"), Some(-1000));
        assert_eq!(parse_date("not a date"), None);
        assert_eq!(parse_date("2026-13-01"), None);
    }

    // Values from `TZ=UTC node -e 'Date.parse(s)'`; each one was misread by one of the copies jsrt replaces.
    #[test]
    fn jsrt_date_parse_matches_v8_where_the_copies_disagreed() {
        // review/ledger and services/local accepted these; V8 is NaN.
        assert_eq!(parse_date("2026-10-07T24:30Z"), None);
        assert_eq!(parse_date("2026-10-07T24:00:30Z"), None);
        assert_eq!(parse_date("2026-10-07T19:43:49+25:00"), None);
        // ai/model_refresh dropped the offset after a fraction (two hours off) and refused it without one.
        assert_eq!(parse_date("2026-10-07T19:43:49.517+02:00"), Some(1_791_395_029_517));
        assert_eq!(parse_date("2026-10-07T19:43:49-05:30"), Some(1_791_422_029_000));
        assert_eq!(parse_date("2026-10-07T19:43:49+00:00"), Some(1_791_402_229_000));
        // ...accepted a leap second, a doubled `Z`, an empty fraction and a signed hour.
        assert_eq!(parse_date("2026-10-07T23:59:60Z"), None);
        assert_eq!(parse_date("2026-10-07T19:43:49ZZ"), None);
        assert_eq!(parse_date("2026-10-07T19:43:49.Z"), None);
        assert_eq!(parse_date("2026-10-07T+1:00:00"), None);
        // ...and refused a date or a minute-precision time.
        assert_eq!(parse_date("2026-10-07"), Some(1_791_331_200_000));
        assert_eq!(parse_date("2026-10-07T19:43Z"), Some(1_791_402_180_000));
        // The ledger refused the `±HHmm` offset and the short forms.
        assert_eq!(parse_date("2026-10-07T19:43:49+0200"), Some(1_791_395_029_000));
        assert_eq!(parse_date("2026-1-5"), Some(1_767_571_200_000));
        assert_eq!(parse_date("2026-10"), Some(1_790_812_800_000));
        // Neither form is in V8's ISO grammar.
        assert_eq!(parse_date("Thu, 08 Oct 2026 19:43:49 GMT"), None);
        assert_eq!(parse_date("2026-10-07T19:43:49UTC"), None);
        assert_eq!(parse_date("-000000-01-01T00:00:00Z"), None);
    }

    // Ported from session-file/tests/upstream.rs and tui/tests/render_debug.rs.
    #[test]
    fn jsrt_date_iso_string_matches_to_iso_string() {
        let cases = [
            (0, "1970-01-01T00:00:00.000Z"),
            (-1, "1969-12-31T23:59:59.999Z"),
            (1_729_000_000_123, "2024-10-15T13:46:40.123Z"),
            (1_735_689_600_123, "2025-01-01T00:00:00.123Z"),
            (1_792_224_000_999, "2026-10-17T08:00:00.999Z"),
            (253_402_300_799_999, "9999-12-31T23:59:59.999Z"),
            (253_402_300_800_000, "+010000-01-01T00:00:00.000Z"),
            (-62_167_219_200_000, "0000-01-01T00:00:00.000Z"),
            (-62_167_219_200_001, "-000001-12-31T23:59:59.999Z"),
            (-62_198_755_200_000, "-000001-01-01T00:00:00.000Z"),
            (8_640_000_000_000_000, "+275760-09-13T00:00:00.000Z"),
            (-8_640_000_000_000_000, "-271821-04-20T00:00:00.000Z"),
        ];
        for (ms, iso) in cases {
            assert_eq!(iso_string(ms), iso, "{ms}");
            assert_eq!(parse_date(iso), Some(ms), "{iso}");
        }
    }

    #[test]
    fn jsrt_date_time_clip_is_the_range_a_date_holds() {
        assert_eq!(time_clip(1.9), Some(1));
        assert_eq!(time_clip(-0.5), Some(0));
        assert_eq!(time_clip(8.64e15), Some(8_640_000_000_000_000));
        assert_eq!(time_clip(8.64e15 + 1.0), None);
        assert_eq!(time_clip(f64::NAN), None);
        assert_eq!(time_clip(f64::NEG_INFINITY), None);
    }

    #[test]
    fn jsrt_date_civil_days_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(days_from_civil(2000, 2, 29), 11_016);
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(days_from_civil(2019, 2, 30), days_from_civil(2019, 3, 2));
        for days in [-719_468 - 1, -719_468, -1, 0, 59, 365, 11_016, 2_932_896, 100_000_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{days}");
        }
    }

    // `fs.statSync(p).mtimeMs` from Node 26 for files touched to these times.
    #[test]
    fn jsrt_date_mtime_ms_matches_node_stats() {
        use std::time::Duration;
        let at = |secs: u64, nanos: u32| UNIX_EPOCH + Duration::new(secs, nanos);
        let before = |secs: u64, nanos: u32| UNIX_EPOCH - Duration::new(secs, nanos);
        assert_eq!(mtime_ms(at(1_791_402_229, 517_122_983)), 1_791_402_229_517.123);
        assert_eq!(mtime_ms(UNIX_EPOCH), 0.0);
        assert_eq!(mtime_ms(before(1, 500_000_000)), -1500.0);
        assert_eq!(mtime_ms(before(0, 1)), -9.999_999_974_752_427e-7);
    }

    #[test]
    fn jsrt_date_now_is_whole_milliseconds_after_2020() {
        let now = now_ms();
        assert!(now > 1_577_836_800_000, "{now}");
    }
}
