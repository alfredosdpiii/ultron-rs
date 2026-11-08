//! SHA-256 (`crypto.createHash("sha256")`), lowercase hex, HMAC-SHA256.

use sha2::{Digest, Sha256};

/// Lowercase hex of `bytes` (`Buffer.toString("hex")`).
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub fn sha256(data: impl AsRef<[u8]>) -> [u8; 32] {
    Sha256::digest(data.as_ref()).into()
}

/// `createHash("sha256").update(data).digest("hex")`: 64 lowercase hex digits.
pub fn sha256_hex(data: impl AsRef<[u8]>) -> String {
    hex(&sha256(data))
}

/// HMAC-SHA256 (RFC 2104): `createHmac("sha256", key).update(data).digest()`.
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > block.len() {
        block[..32].copy_from_slice(&sha256(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|value| value ^ byte);
    let inner = Sha256::new().chain_update(pad(0x36)).chain_update(data).finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsrt_sha256_hex_is_lowercase_fips_180_4() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(hex(&sha256(b"abc")), sha256_hex(b"abc"));
    }

    #[test]
    fn jsrt_hex_pads_and_lowercases() {
        assert_eq!(hex(&[0x00, 0x0a, 0xff, 0xAB]), "000affab");
        assert_eq!(hex(&[]), "");
    }

    /// RFC 4231 test cases 1, 2 and 6 (a key longer than the block is hashed first).
    #[test]
    fn jsrt_hmac_sha256_matches_rfc_4231() {
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }
}
