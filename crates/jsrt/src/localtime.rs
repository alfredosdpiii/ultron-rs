//! Local time for printed dates (`Date#getHours` and friends): the zone named by `TZ` (a zoneinfo name or a POSIX
//! rule), else `/etc/localtime`, read from TZif files (RFC 8536) with the footer rule for times after the last
//! transition. Anything unreadable is UTC.

use std::path::{Path, PathBuf};

use crate::date::{civil_from_days, days_from_civil};

/// Calendar fields of a time: year, month (1-12), day, hour, minute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fields {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The calendar fields of `ms` (milliseconds since the epoch) shifted by `offset_seconds`.
pub fn fields(ms: f64, offset_seconds: i64) -> Fields {
    let total = (ms / 1000.0).floor() as i64 + offset_seconds;
    let days = total.div_euclid(86_400);
    let secs = total.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Fields {
        year,
        month,
        day,
        hour: (secs / 3600) as u32,
        minute: ((secs % 3600) / 60) as u32,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Day {
    /// `Jn`: 1-365, February 29 never counted.
    Julian(u32),
    /// `n`: 0-365, February 29 counted.
    Zero(u32),
    /// `Mm.w.d`.
    Month(u32, u32, u32),
}

/// A transition rule's day and its time of day in seconds.
type RuleTime = (Day, i64);

#[derive(Clone, Debug, PartialEq)]
struct PosixRule {
    std_offset: i64,
    dst: Option<(i64, RuleTime, RuleTime)>,
}

fn parse_name(s: &str) -> Option<&str> {
    if let Some(rest) = s.strip_prefix('<') {
        let end = rest.find('>')?;
        return Some(&rest[end + 1..]);
    }
    let end = s.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(s.len());
    (end >= 3).then(|| &s[end..])
}

/// `[+-]hh[:mm[:ss]]` in seconds, and the rest.
fn parse_time(s: &str) -> Option<(i64, &str)> {
    let (sign, s) = match s.as_bytes().first() {
        Some(b'-') => (-1, &s[1..]),
        Some(b'+') => (1, &s[1..]),
        _ => (1, s),
    };
    let mut total = 0;
    let mut rest = s;
    for (i, scale) in [3600, 60, 1].into_iter().enumerate() {
        if i > 0 {
            match rest.strip_prefix(':') {
                Some(r) => rest = r,
                None => break,
            }
        }
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        total += rest[..end].parse::<i64>().ok()? * scale;
        rest = &rest[end..];
    }
    Some((sign * total, rest))
}

fn parse_date(s: &str) -> Option<((Day, i64), &str)> {
    let number = |s: &str| -> Option<(u32, usize)> {
        let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        Some((s[..end].parse().ok()?, end))
    };
    let (day, mut rest) = if let Some(r) = s.strip_prefix('M') {
        let (m, a) = number(r)?;
        let r = r[a..].strip_prefix('.')?;
        let (w, b) = number(r)?;
        let r = r[b..].strip_prefix('.')?;
        let (d, c) = number(r)?;
        (Day::Month(m, w, d), &r[c..])
    } else if let Some(r) = s.strip_prefix('J') {
        let (n, a) = number(r)?;
        (Day::Julian(n), &r[a..])
    } else {
        let (n, a) = number(s)?;
        (Day::Zero(n), &s[a..])
    };
    let mut time = 7200;
    if let Some(r) = rest.strip_prefix('/') {
        let (t, r) = parse_time(r)?;
        time = t;
        rest = r;
    }
    Some(((day, time), rest))
}

fn parse_posix(s: &str) -> Option<PosixRule> {
    let rest = parse_name(s)?;
    let (std_west, rest) = parse_time(rest)?;
    if rest.is_empty() {
        return Some(PosixRule {
            std_offset: -std_west,
            dst: None,
        });
    }
    let rest = parse_name(rest)?;
    let (dst_west, rest) = if rest.is_empty() || rest.starts_with(',') {
        (std_west - 3600, rest)
    } else {
        parse_time(rest)?
    };
    let rules = if rest.is_empty() { ",M3.2.0,M11.1.0" } else { rest };
    let (start, rest) = parse_date(rules.strip_prefix(',')?)?;
    let (end, rest) = parse_date(rest.strip_prefix(',')?)?;
    rest.is_empty().then_some(PosixRule {
        std_offset: -std_west,
        dst: Some((-dst_west, start, end)),
    })
}

/// Seconds since the epoch of the rule's day in `year`, at local midnight (as UTC).
fn rule_day(year: i64, day: Day) -> i64 {
    let jan1 = days_from_civil(year, 1, 1);
    let days = match day {
        Day::Julian(n) => jan1 + i64::from(n) - 1 + i64::from(is_leap(year) && n >= 60),
        Day::Zero(n) => jan1 + i64::from(n),
        Day::Month(m, w, d) => {
            let first = days_from_civil(year, m, 1);
            let weekday = (first + 4).rem_euclid(7) as u32;
            let mut day = 1 + (d + 7 - weekday) % 7 + (w.max(1) - 1) * 7;
            while day > days_in_month(year, m) {
                day -= 7;
            }
            first + i64::from(day) - 1
        }
    };
    days * 86_400
}

impl PosixRule {
    fn offset(&self, t: i64) -> i64 {
        let Some((dst_offset, (start_day, start_time), (end_day, end_time))) = self.dst else {
            return self.std_offset;
        };
        let year = fields((t + self.std_offset) as f64 * 1000.0, 0).year;
        let start = rule_day(year, start_day) + start_time - self.std_offset;
        let end = rule_day(year, end_day) + end_time - dst_offset;
        let dst = if start < end {
            start <= t && t < end
        } else {
            !(end <= t && t < start)
        };
        if dst { dst_offset } else { self.std_offset }
    }
}

/// A time zone: TZif transitions and types, and the rule for later times.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Zone {
    transitions: Vec<(i64, usize)>,
    offsets: Vec<i64>,
    footer: Option<PosixRule>,
}

fn be(bytes: &[u8], at: usize, len: usize) -> Option<i64> {
    let slice = bytes.get(at..at + len)?;
    let mut value: i64 = 0;
    for b in slice {
        value = (value << 8) | i64::from(*b);
    }
    let shift = 64 - 8 * len as u32;
    Some((value << shift) >> shift)
}

impl Zone {
    pub fn utc() -> Zone {
        Zone::default()
    }

    pub fn from_tzif(bytes: &[u8]) -> Option<Zone> {
        if bytes.get(..4)? != b"TZif" {
            return None;
        }
        let counts = |at: usize| -> Option<[usize; 6]> {
            let mut c = [0; 6];
            for (i, slot) in c.iter_mut().enumerate() {
                *slot = be(bytes, at + 20 + 4 * i, 4)? as usize;
            }
            Some(c)
        };
        let [isut, isstd, leap, time, types, chars] = counts(0)?;
        let (base, size) = if bytes[4] >= b'2' {
            let v1 = 44 + time * 5 + types * 6 + chars + leap * 8 + isstd + isut;
            (v1, 8)
        } else {
            (0, 4)
        };
        let [isut, isstd, leap, time, types, chars] = counts(base)?;
        let data = base + 44;
        let mut zone = Zone::default();
        for i in 0..time {
            let at = be(bytes, data + i * size, size)?;
            let kind = *bytes.get(data + time * size + i)? as usize;
            zone.transitions.push((at, kind));
        }
        let types_at = data + time * (size + 1);
        for i in 0..types {
            zone.offsets.push(be(bytes, types_at + i * 6, 4)?);
        }
        if size == 8 {
            let end = types_at + types * 6 + chars + leap * 12 + isstd + isut;
            let footer = bytes.get(end..)?;
            let text = std::str::from_utf8(footer).ok()?.trim_matches('\n');
            zone.footer = parse_posix(text);
        }
        Some(zone)
    }

    /// The zone of a POSIX TZ string such as `EST5EDT,M3.2.0,M11.1.0`.
    pub fn from_posix(text: &str) -> Option<Zone> {
        Some(Zone {
            footer: Some(parse_posix(text)?),
            ..Zone::default()
        })
    }

    /// The zone the process runs in: `TZ` (a zoneinfo name or a POSIX rule), else `/etc/localtime`.
    pub fn from_env(tz: Option<&str>) -> Zone {
        let read = |path: &Path| std::fs::read(path).ok().and_then(|b| Zone::from_tzif(&b));
        match tz {
            None => read(Path::new("/etc/localtime")).unwrap_or_default(),
            Some(tz) => {
                let name = tz.strip_prefix(':').unwrap_or(tz);
                if name.is_empty() {
                    return Zone::utc();
                }
                let path = if name.starts_with('/') {
                    PathBuf::from(name)
                } else {
                    Path::new("/usr/share/zoneinfo").join(name)
                };
                (!name.contains(".."))
                    .then(|| read(&path))
                    .flatten()
                    .or_else(|| Zone::from_posix(name))
                    .unwrap_or_default()
            }
        }
    }

    /// The offset from UTC, in seconds, at `ms` since the epoch.
    pub fn offset(&self, ms: f64) -> i64 {
        let t = (ms / 1000.0).floor() as i64;
        if self.transitions.is_empty()
            && let Some(footer) = &self.footer
        {
            return footer.offset(t);
        }
        match self.transitions.iter().rposition(|(at, _)| *at <= t) {
            None => self.offsets.first().copied().unwrap_or(0),
            Some(i) if i == self.transitions.len() - 1 && self.footer.is_some() => {
                self.footer.as_ref().map_or(0, |f| f.offset(t))
            }
            Some(i) => self.offsets.get(self.transitions[i].1).copied().unwrap_or(0),
        }
    }

    /// The local calendar fields of `ms`.
    pub fn fields(&self, ms: f64) -> Fields {
        fields(ms, self.offset(ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> Fields {
        Fields {
            year,
            month,
            day,
            hour,
            minute,
        }
    }

    // Ported from cli/tests/subcommand_units.rs `cli_localtime_posix_rules_match_node` (values from Node under TZ).
    #[test]
    fn jsrt_localtime_posix_rules_match_node() {
        let ny = Zone::from_posix("EST5EDT,M3.2.0,M11.1.0").unwrap();
        // 2026-03-08T06:59:59Z is 01:59 EST; 07:00Z is 03:00 EDT.
        assert_eq!(ny.fields(1_772_953_199_000.0), at(2026, 3, 8, 1, 59));
        assert_eq!(ny.fields(1_772_953_200_000.0), at(2026, 3, 8, 3, 0));
        // 2026-11-01T06:00:00Z is 01:00 EST (fall back at 06:00Z); 05:59Z is 01:59 EDT.
        assert_eq!(ny.fields(1_793_512_740_000.0), at(2026, 11, 1, 1, 59));
        assert_eq!(ny.fields(1_793_512_800_000.0), at(2026, 11, 1, 1, 0));
        // Southern hemisphere: 2026-01-01T00:00Z is 11:00 AEDT.
        let sydney = Zone::from_posix("AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        assert_eq!(sydney.fields(1_767_225_600_000.0).hour, 11);
        assert_eq!(sydney.fields(1_782_864_000_000.0).hour, 10);
        // A fixed offset with a quoted name, and a fractional one.
        assert_eq!(
            Zone::from_posix("<+0530>-5:30").unwrap().fields(0.0),
            at(1970, 1, 1, 5, 30)
        );
        assert_eq!(Zone::utc().fields(-1.0), at(1969, 12, 31, 23, 59));
    }

    // Ported from cli/tests/subcommand_units.rs `cli_localtime_reads_zoneinfo_files`.
    #[test]
    fn jsrt_localtime_reads_zoneinfo_files() {
        if !Path::new("/usr/share/zoneinfo/America/New_York").exists() {
            return;
        }
        let zone = Zone::from_env(Some("America/New_York"));
        assert_eq!(zone.offset(1_772_953_200_000.0), -4 * 3600);
        assert_eq!(zone.offset(1_772_953_199_000.0), -5 * 3600);
        // 1990 is inside the transition table; 2090 relies on the footer rule.
        assert_eq!(zone.offset(647_100_000_000.0), -4 * 3600);
        assert_eq!(zone.offset(3_802_982_400_000.0), -4 * 3600);
        assert_eq!(zone.offset(3_787_862_400_000.0), -5 * 3600);
        assert_eq!(Zone::from_env(Some("Nowhere/Invalid")).offset(0.0), 0);
        assert_eq!(Zone::from_env(Some(":UTC")).offset(0.0), 0);
    }

    #[test]
    fn jsrt_localtime_utc_fields_match_format_clock() {
        assert_eq!(fields(1_791_208_239_000.0, 0), at(2026, 10, 5, 13, 50));
        assert_eq!(fields(1_791_201_600_000.0, 0), at(2026, 10, 5, 12, 0));
        assert_eq!(fields(0.0, 0), at(1970, 1, 1, 0, 0));
        assert_eq!(fields(951_782_400_000.0, 0), at(2000, 2, 29, 0, 0));
    }
}
