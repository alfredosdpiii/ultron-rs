//! `utils/hash.ts`.

/// `shortHash`: a fast deterministic 53-bit hash of the UTF-16 code units, in base 36.
pub fn short_hash(text: &str) -> String {
    let mut h1: u32 = 0xdeadbeef;
    let mut h2: u32 = 0x41c6ce57;
    for unit in text.encode_utf16() {
        let ch = u32::from(unit);
        h1 = (h1 ^ ch).wrapping_mul(2654435761);
        h2 = (h2 ^ ch).wrapping_mul(1597334677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2246822507) ^ (h2 ^ (h2 >> 13)).wrapping_mul(3266489909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2246822507) ^ (h1 ^ (h1 >> 13)).wrapping_mul(3266489909);
    format!("{}{}", radix36(h2), radix36(h1))
}

fn radix36(mut n: u32) -> String {
    if n == 0 {
        return "0".into();
    }
    let mut digits = Vec::new();
    while n > 0 {
        digits.push(char::from_digit(n % 36, 36).unwrap_or('0'));
        n /= 36;
    }
    digits.iter().rev().collect()
}
