//! PKCE (RFC 7636, S256) and random strings.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::{Error, Result};

/// `n` random bytes, base64url without padding.
pub(crate) fn random_urlsafe(n: usize) -> Result<String> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).map_err(|e| Error::Auth(format!("no system randomness: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

/// A code verifier: 32 random bytes → 43 base64url characters (256 bits).
pub(crate) fn new_verifier() -> Result<String> {
    random_urlsafe(32)
}

/// `BASE64URL(SHA256(verifier))`.
pub(crate) fn challenge_s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7636_appendix_b_vector() {
        assert_eq!(
            challenge_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_shape() {
        let v = new_verifier().unwrap();
        assert_eq!(v.len(), 43);
        assert!(
            v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert_ne!(v, new_verifier().unwrap());
    }
}
