//! PKCE (RFC 7636, S256) and the state cookie that carries the verifier
//! between `start` and `callback`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// 32 random bytes, base64url without padding (43 chars). `None` when the OS
/// RNG is unavailable.
pub fn random_token() -> Option<String> {
    let mut raw = [0u8; 32];
    getrandom::getrandom(&mut raw).ok()?;
    Some(URL_SAFE_NO_PAD.encode(raw))
}

/// `code_challenge` for `code_challenge_method=S256`.
pub fn challenge_s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// State cookie value `<state>.<verifier>`; base64url never contains `.`.
pub fn encode_state_cookie(state: &str, verifier: &str) -> String {
    format!("{state}.{verifier}")
}

/// Splits a state cookie value. `None` for anything not produced by
/// [`encode_state_cookie`], including the pre-PKCE single-token cookie.
pub fn decode_state_cookie(value: &str) -> Option<(&str, &str)> {
    let (state, verifier) = value.split_once('.')?;
    if state.is_empty() || verifier.len() != 43 || verifier.contains('.') {
        return None;
    }
    Some((state, verifier))
}
