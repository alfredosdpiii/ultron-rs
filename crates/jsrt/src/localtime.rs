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

fn parse_u32_prefix(s: &str) -> Option<(u32, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let value = s[..end].parse().ok()?;
    Some((value, &s[end..]))
}

fn parse_month_rule(s: &str) -> Option<(Day, &str)> {
    let (month, rest) = parse_u32_prefix(s)?;
    let (week, rest) = parse_u32_prefix(rest.strip_prefix('.')?)?;
    let (weekday, rest) = parse_u32_prefix(rest.strip_prefix('.')?)?;
    Some((Day::Month(month, week, weekday), rest))
}

fn parse_rule_day(s: &str) -> Option<(Day, &str)> {
    if let Some(rest) = s.strip_prefix('M') {
        return parse_month_rule(rest);
    }
    if let Some(rest) = s.strip_prefix('J') {
        let (day, rest) = parse_u32_prefix(rest)?;
        return Some((Day::Julian(day), rest));
    }
    let (day, rest) = parse_u32_prefix(s)?;
    Some((Day::Zero(day), rest))
}

fn parse_date(s: &str) -> Option<((Day, i64), &str)> {
    let (day, rest) = parse_rule_day(s)?;
    let (time, rest) = match rest.strip_prefix('/') {
        Some(after_slash) => parse_time(after_slash)?,
        None => (7200, rest),
    };
    Some(((day, time), rest))
}

fn parse_dst_transitions(rest: &str) -> Option<(RuleTime, RuleTime)> {
    let rules = if rest.is_empty() { ",M3.2.0,M11.1.0" } else { rest };
    let (start, rest) = parse_date(rules.strip_prefix(',')?)?;
    let (end, rest) = parse_date(rest.strip_prefix(',')?)?;
    rest.is_empty().then_some((start, end))
}

fn parse_dst_rule(std_west: i64, rest: &str) -> Option<(i64, RuleTime, RuleTime)> {
    let rest = parse_name(rest)?;
    let (dst_west, rest) = if rest.is_empty() || rest.starts_with(',') {
        (std_west - 3600, rest)
    } else {
        parse_time(rest)?
    };
    let (start, end) = parse_dst_transitions(rest)?;
    Some((-dst_west, start, end))
}

fn parse_posix(s: &str) -> Option<PosixRule> {
    let rest = parse_name(s)?;
    let (std_west, rest) = parse_time(rest)?;
    let dst = if rest.is_empty() {
        None
    } else {
        Some(parse_dst_rule(std_west, rest)?)
    };
    Some(PosixRule {
        std_offset: -std_west,
        dst,
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

struct TzifSection {
    base: usize,
    size: usize,
    counts: [usize; 6],
}

fn tzif_counts(bytes: &[u8], at: usize) -> Option<[usize; 6]> {
    let mut counts = [0; 6];
    for (index, slot) in counts.iter_mut().enumerate() {
        *slot = be(bytes, at + 20 + 4 * index, 4)? as usize;
    }
    Some(counts)
}

fn tzif_section(bytes: &[u8]) -> Option<TzifSection> {
    if bytes.get(..4)? != b"TZif" {
        return None;
    }
    let first = tzif_counts(bytes, 0)?;
    if bytes[4] < b'2' {
        return Some(TzifSection {
            base: 0,
            size: 4,
            counts: first,
        });
    }
    let [isut, isstd, leap, time, types, chars] = first;
    let base = 44 + time * 5 + types * 6 + chars + leap * 8 + isstd + isut;
    Some(TzifSection {
        base,
        size: 8,
        counts: tzif_counts(bytes, base)?,
    })
}

fn read_tzif_transitions(bytes: &[u8], section: &TzifSection) -> Option<Vec<(i64, usize)>> {
    let data = section.base + 44;
    let count = section.counts[3];
    let size = section.size;
    let mut transitions = Vec::with_capacity(count);
    for index in 0..count {
        let at = be(bytes, data + index * size, size)?;
        let kind = *bytes.get(data + count * size + index)? as usize;
        transitions.push((at, kind));
    }
    Some(transitions)
}

fn read_tzif_offsets(bytes: &[u8], types_at: usize, count: usize) -> Option<Vec<i64>> {
    let mut offsets = Vec::with_capacity(count);
    for index in 0..count {
        offsets.push(be(bytes, types_at + index * 6, 4)?);
    }
    Some(offsets)
}

fn read_tzif_footer(bytes: &[u8], end: usize) -> Option<Option<PosixRule>> {
    let footer = bytes.get(end..)?;
    let text = std::str::from_utf8(footer).ok()?.trim_matches('\n');
    Some(parse_posix(text))
}

impl Zone {
    pub fn utc() -> Zone {
        Zone::default()
    }

    pub fn from_tzif(bytes: &[u8]) -> Option<Zone> {
        let section = tzif_section(bytes)?;
        let [isut, isstd, leap, time, types, chars] = section.counts;
        let types_at = section.base + 44 + time * (section.size + 1);
        let transitions = read_tzif_transitions(bytes, &section)?;
        let offsets = read_tzif_offsets(bytes, types_at, types)?;
        let footer = if section.size == 8 {
            let end = types_at + types * 6 + chars + leap * 12 + isstd + isut;
            read_tzif_footer(bytes, end)?
        } else {
            None
        };
        Some(Zone {
            transitions,
            offsets,
            footer,
        })
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

    /// The first UTC epoch timestamp (ms) strictly after `after_ms` whose local wall clock is `hour:minute`.
    /// Recomputes the zone offset for the target calendar day so DST transitions do not drift by an hour.
    pub fn next_local_time(&self, after_ms: i64, hour: u32, minute: u32) -> i64 {
        let local = self.fields(after_ms as f64);
        let today = days_from_civil(local.year, local.month, local.day);
        let target_secs = i64::from(hour * 60 + minute) * 60;
        for delta in 0..=2 {
            let local_secs = (today + delta) * 86_400 + target_secs;
            let approx = local_secs - self.offset((local_secs as f64) * 1000.0);
            let off = self.offset((approx as f64) * 1000.0);
            let utc = local_secs - off;
            let verify = self.offset((utc as f64) * 1000.0);
            let resolved = if verify == off {
                utc
            } else {
                let alt = local_secs - verify;
                if self.offset((alt as f64) * 1000.0) == verify {
                    alt
                } else {
                    utc.max(alt)
                }
            };
            let ms = resolved * 1000;
            if ms > after_ms {
                return ms;
            }
        }
        after_ms + 86_400_000
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

    #[test]
    fn jsrt_localtime_posix_rules_match_node() {
        let ny = Zone::from_posix("EST5EDT,M3.2.0,M11.1.0").unwrap();
        assert_eq!(ny.fields(1_772_953_199_000.0), at(2026, 3, 8, 1, 59));
        assert_eq!(ny.fields(1_772_953_200_000.0), at(2026, 3, 8, 3, 0));
        assert_eq!(ny.fields(1_793_512_740_000.0), at(2026, 11, 1, 1, 59));
        assert_eq!(ny.fields(1_793_512_800_000.0), at(2026, 11, 1, 1, 0));
        let sydney = Zone::from_posix("AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        assert_eq!(sydney.fields(1_767_225_600_000.0).hour, 11);
        assert_eq!(sydney.fields(1_782_864_000_000.0).hour, 10);
        assert_eq!(
            Zone::from_posix("<+0530>-5:30").unwrap().fields(0.0),
            at(1970, 1, 1, 5, 30)
        );
        assert_eq!(Zone::utc().fields(-1.0), at(1969, 12, 31, 23, 59));
    }

    #[test]
    fn jsrt_localtime_reads_zoneinfo_files() {
        if !Path::new("/usr/share/zoneinfo/America/New_York").exists() {
            return;
        }
        let zone = Zone::from_env(Some("America/New_York"));
        assert_eq!(zone.offset(1_772_953_200_000.0), -4 * 3600);
        assert_eq!(zone.offset(1_772_953_199_000.0), -5 * 3600);
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

    #[test]
    fn jsrt_localtime_next_local_time_tracks_dst_transitions() {
        let ny = Zone::from_posix("EST5EDT,M3.2.0,M11.1.0").unwrap();
        let mar7_0330_est = 1_772_872_200_000;
        assert_eq!(ny.fields(mar7_0330_est as f64), at(2026, 3, 7, 3, 30));
        let mar8_0330_edt = ny.next_local_time(mar7_0330_est, 3, 30);
        assert_eq!(ny.fields(mar8_0330_edt as f64), at(2026, 3, 8, 3, 30));
        assert_eq!(mar8_0330_edt - mar7_0330_est, 23 * 3_600_000);

        let oct31_0330_edt = 1_793_431_800_000;
        assert_eq!(ny.fields(oct31_0330_edt as f64), at(2026, 10, 31, 3, 30));
        let nov1_0330_est = ny.next_local_time(oct31_0330_edt, 3, 30);
        assert_eq!(ny.fields(nov1_0330_est as f64), at(2026, 11, 1, 3, 30));
        assert_eq!(nov1_0330_est - oct31_0330_edt, 25 * 3_600_000);
    }
}
