//! Digests that tell values apart without holding them.

use sha2::{Digest, Sha256};

/// The SHA-256 of `parts` run together, in hex.
pub fn sha256_hex(parts: &[&[u8]]) -> String {
    let digest = Sha256::digest(parts.concat());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
