//! OS randomness, random hex, `crypto.randomUUID()` and the monotonic `uuidv7()` of `@ultron/ai`.

use std::sync::{Mutex, PoisonError};

/// Fill `buf` from the OS random source. Panics when it is unavailable, as `crypto.getRandomValues` throws: an
/// all-zero id or token must never be handed out silently.
pub fn fill(buf: &mut [u8]) {
    getrandom::fill(buf).expect("the OS random source is unavailable");
}

pub fn bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    fill(&mut out);
    out
}

/// `randomBytes(n).toString("hex")`: `2 * n` lowercase hex digits.
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    fill(&mut buf);
    crate::hash::hex(&buf)
}

/// The canonical 8-4-4-4-12 lowercase form of 16 bytes.
pub fn format_uuid(b: &[u8; 16]) -> String {
    let hex = crate::hash::hex(b);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// A version-4 UUID from 16 random bytes (version and variant bits overwritten).
pub fn uuid_v4_from(mut b: [u8; 16]) -> String {
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format_uuid(&b)
}

/// `crypto.randomUUID()`.
pub fn uuid_v4() -> String {
    uuid_v4_from(bytes())
}

const MAX_UUID_V7_TIMESTAMP: i64 = 0xffff_ffff_ffff;
const MAX_SEQUENCE: u64 = (1 << 41) - 1;

/// The `uuidv7()` generator of `@ultron/ai`: 48 bits of Unix milliseconds that never go backwards, then a 41-bit
/// sequence seeded from the first id's random bytes and incremented for every id, so the ids of one generator
/// sort in creation order. The remaining bits are random.
#[derive(Debug)]
pub struct Uuidv7 {
    last_ordinary: i64,
    sequence: Option<u64>,
}

impl Default for Uuidv7 {
    fn default() -> Self {
        Self::new()
    }
}

impl Uuidv7 {
    pub const fn new() -> Self {
        Uuidv7 {
            last_ordinary: -1,
            sequence: None,
        }
    }

    /// An id at `explicit` when given (a follower id: the ordinary clock is unaffected), else at `now` clamped
    /// to never go before the previous ordinary id. `bytes` supplies the random bits (zeros in fixtures).
    pub fn generate(&mut self, explicit: Option<i64>, now: i64, mut bytes: [u8; 16]) -> String {
        let timestamp = match explicit {
            Some(at) => at,
            None => {
                self.last_ordinary = now.max(self.last_ordinary);
                self.last_ordinary
            }
        }
        .clamp(0, MAX_UUID_V7_TIMESTAMP);
        let sequence = match self.sequence {
            None => bytes[1..6].iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)),
            // ponytail: upstream throws once 2^41 ids share one generator; wrap instead of failing a session.
            Some(s) if s == MAX_SEQUENCE => 0,
            Some(s) => s + 1,
        };
        self.sequence = Some(sequence);
        let ts = timestamp as u64;
        for (index, byte) in bytes.iter_mut().take(6).enumerate() {
            *byte = (ts >> ((5 - index) * 8)) as u8;
        }
        bytes[6] = 0x70 | ((sequence >> 37) & 0x0f) as u8;
        bytes[7] = (sequence >> 29) as u8;
        bytes[8] = 0x80 | ((sequence >> 23) & 0x3f) as u8;
        bytes[9] = (sequence >> 15) as u8;
        bytes[10] = (sequence >> 7) as u8;
        bytes[11] = (((sequence & 0x7f) << 1) as u8) | (bytes[11] & 0x01);
        format_uuid(&bytes)
    }
}

/// The process-wide generator (upstream's generator state is module-global).
static GENERATOR: Mutex<Uuidv7> = Mutex::new(Uuidv7::new());

/// `uuidv7()` at `now_ms` from the process-wide generator.
pub fn uuid_v7(now_ms: i64) -> String {
    let mut generator = GENERATOR.lock().unwrap_or_else(PoisonError::into_inner);
    generator.generate(None, now_ms, bytes())
}

/// `uuidv7(at)`: an id whose timestamp is exactly `at`, from the process-wide generator's sequence.
pub fn uuid_v7_exact(at_ms: i64) -> String {
    let mut generator = GENERATOR.lock().unwrap_or_else(PoisonError::into_inner);
    generator.generate(Some(at_ms), 0, bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_uuid(id: &str, version: u8) -> bool {
        let b = id.as_bytes();
        id.len() == 36
            && [8, 13, 18, 23].iter().all(|i| b[*i] == b'-')
            && b[14] == version
            && matches!(b[19], b'8' | b'9' | b'a' | b'b')
            && id
                .chars()
                .filter(|c| *c != '-')
                .all(|c| matches!(c, '0'..='9' | 'a'..='f'))
    }

    #[test]
    fn jsrt_uuid_v4_sets_version_and_variant() {
        assert_eq!(uuid_v4_from([0; 16]), "00000000-0000-4000-8000-000000000000");
        assert_eq!(uuid_v4_from([0xff; 16]), "ffffffff-ffff-4fff-bfff-ffffffffffff");
        let (a, b) = (uuid_v4(), uuid_v4());
        assert!(is_uuid(&a, b'4') && is_uuid(&b, b'4') && a != b, "{a} {b}");
    }

    #[test]
    fn jsrt_random_hex_has_two_digits_per_byte() {
        let h = random_hex(24);
        assert_eq!(h.len(), 48);
        assert!(h.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        assert_eq!(random_hex(0), "");
    }

    /// From session-file's `session_file_uuid_v7_is_monotonic_and_versioned`.
    #[test]
    fn jsrt_uuid_v7_is_monotonic_and_versioned() {
        let mut g = Uuidv7::default();
        let a = g.generate(None, 1_735_787_045_678, [0x11; 16]);
        let b = g.generate(None, 1_735_787_045_000, [0x22; 16]);
        assert!(is_uuid(&a, b'7') && is_uuid(&b, b'7'));
        assert_eq!(&a[..13], "019424f8-632e");
        assert_eq!(
            &b[..13],
            "019424f8-632e",
            "clock going backwards keeps the last timestamp"
        );
        assert!(b > a);
    }

    /// The fixture generators pin the random bytes to zeros: the sequence starts at 0 and counts up.
    #[test]
    fn jsrt_uuid_v7_with_zero_randomness_counts_the_sequence() {
        let mut g = Uuidv7::new();
        assert_eq!(
            g.generate(None, 0x0123_4567_89ab, [0; 16]),
            "01234567-89ab-7000-8000-000000000000"
        );
        assert_eq!(
            g.generate(None, 0x0123_4567_89ab, [0; 16]),
            "01234567-89ab-7000-8000-000200000000"
        );
    }

    /// A follower id takes its exact time and does not move the ordinary clock.
    #[test]
    fn jsrt_uuid_v7_exact_leaves_the_ordinary_clock() {
        let mut g = Uuidv7::new();
        let follower = g.generate(Some(2_000), 0, [0; 16]);
        assert!(follower.starts_with("00000000-07d0-7"), "{follower}");
        let ordinary = g.generate(None, 1_000, [0; 16]);
        assert!(ordinary.starts_with("00000000-03e8-7"), "{ordinary}");
        assert!(g.generate(None, -5, [0; 16]).starts_with("00000000-03e8-7"));
    }

    #[test]
    fn jsrt_uuid_v7_clamps_the_timestamp_to_48_bits() {
        let mut g = Uuidv7::new();
        assert!(g.generate(Some(-1), 0, [0; 16]).starts_with("00000000-0000-7"));
        assert!(g.generate(Some(i64::MAX), 0, [0; 16]).starts_with("ffffffff-ffff-7"));
    }

    #[test]
    fn jsrt_process_uuid_v7_sorts_in_creation_order() {
        let ids: Vec<String> = (0..50).map(|_| uuid_v7(1_735_787_045_678)).collect();
        assert!(ids.iter().all(|id| is_uuid(id, b'7')));
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
    }
}
