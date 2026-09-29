//! Two-factor authentication for master admins: TOTP codes (RFC 6238: HMAC-SHA1, 6 digits,
//! 30 s steps), their secrets encrypted at rest, single-use recovery codes, and the enrolment
//! QR code as inline SVG.
//!
//! - **Codes** are accepted one step early or late (clock skew), and only for a step later
//!   than the last one used on that account (`accounts.totp_last_step`), so a code can't be
//!   replayed, not even within its own 30 s.
//! - **Secrets** are sealed with ChaCha20-Poly1305 under a key derived from the master's
//!   signing key (`master.key`, [`game_auth::Identity::derive_key`]); the account id is the
//!   associated data, so a sealed secret copied onto another account doesn't open. A copy of
//!   the database alone (a backup, a leaked file) doesn't reveal them.
//! - **Recovery codes** are 80 random bits each, stored as BLAKE3 hashes (salted with the
//!   account id): enough entropy that a fast hash can't be brute-forced.

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use game_auth::{hex, random_bytes, unhex};
use hmac::{Hmac, Mac};

/// Seconds per code.
pub const STEP_SECS: u64 = 30;
/// Steps accepted before and after the current one.
const SKEW_STEPS: u64 = 1;
/// Bytes in a new secret (160 bits, as RFC 4226 recommends).
pub const SECRET_BYTES: usize = 20;
/// Recovery codes made at enrolment.
pub const RECOVERY_CODES: usize = 10;
/// The `derive_key` context for sealing TOTP secrets.
pub const SEAL_CONTEXT: &str = "bf2r master 2026-09 totp secrets v1";
/// Prefix of a sealed secret (a format version).
const SEALED_PREFIX: &str = "c1:";

/// The HOTP code (RFC 4226) for `counter`, 6 digits.
pub fn hotp(secret: &[u8], counter: u64) -> u32 {
    let mut mac = <Hmac<sha1::Sha1> as Mac>::new_from_slice(secret).expect("HMAC takes keys of any length");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[19] & 0x0f) as usize;
    let value = u32::from_be_bytes([digest[offset] & 0x7f, digest[offset + 1], digest[offset + 2], digest[offset + 3]]);
    value % 1_000_000
}

/// The time step of `unix` seconds.
pub fn step_at(unix: u64) -> u64 {
    unix / STEP_SECS
}

/// The 6-digit code for `step`, zero-padded.
#[cfg(test)]
pub fn code_at(secret: &[u8], step: u64) -> String {
    format!("{:06}", hotp(secret, step))
}

/// The step `code` matches at `now` (within the skew window) that is later than
/// `last_step`, or `None`. Spaces in the code are ignored.
pub fn verify(secret: &[u8], code: &str, now: u64, last_step: u64) -> Option<u64> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let wanted: u32 = code.parse().ok()?;
    let current = step_at(now);
    let mut found = None;
    // Every candidate is computed (no early exit), so timing says nothing about which matched.
    for step in current.saturating_sub(SKEW_STEPS)..=current + SKEW_STEPS {
        if step > last_step && constant_time_eq(&hotp(secret, step).to_be_bytes(), &wanted.to_be_bytes()) {
            found = Some(step);
        }
    }
    found
}

/// Whether `code` looks like a TOTP code (as opposed to a recovery code).
pub fn looks_like_code(code: &str) -> bool {
    let digits: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_digit())
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32, without padding (what authenticator apps expect).
pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &b in bytes {
        buffer = (buffer << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(BASE32[((buffer >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(BASE32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Base32 (any case, spaces and padding ignored) to bytes.
#[cfg(test)]
pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in text.chars().filter(|c| !c.is_whitespace() && *c != '=' && *c != '-') {
        let value = BASE32.iter().position(|&b| b as char == c.to_ascii_uppercase())? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            out.push((buffer >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

/// A new random secret.
pub fn new_secret() -> [u8; SECRET_BYTES] {
    random_bytes()
}

/// The secret in groups of four for reading off the screen: `ABCD EFGH ...`.
pub fn grouped(secret_b32: &str) -> String {
    secret_b32.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" ")
}

/// The `otpauth://` URI authenticator apps import (also what the QR code holds).
pub fn otpauth_uri(issuer: &str, account: &str, secret_b32: &str) -> String {
    let issuer = crate::http::url_encode(issuer);
    format!(
        "otpauth://totp/{issuer}:{}?secret={secret_b32}&issuer={issuer}&algorithm=SHA1&digits=6&period={STEP_SECS}",
        crate::http::url_encode(account)
    )
}

fn cipher(key: &[u8; 32]) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(Key::from_slice(key))
}

/// Encrypts a secret for storage on account `account`.
pub fn seal(key: &[u8; 32], account: u64, secret: &[u8]) -> String {
    let nonce = random_bytes::<12>();
    let sealed = cipher(key)
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: secret, aad: &account.to_le_bytes() })
        .expect("ChaCha20-Poly1305 encrypts any short message");
    format!("{SEALED_PREFIX}{}{}", hex(&nonce), hex(&sealed))
}

/// Decrypts a secret sealed for `account`; `None` if it was sealed for another account,
/// under another key, or changed.
pub fn open(key: &[u8; 32], account: u64, sealed: &str) -> Option<Vec<u8>> {
    let bytes = unhex(sealed.strip_prefix(SEALED_PREFIX)?)?;
    if bytes.len() < 12 + 16 {
        return None;
    }
    let (nonce, ciphertext) = bytes.split_at(12);
    cipher(key).decrypt(Nonce::from_slice(nonce), Payload { msg: ciphertext, aad: &account.to_le_bytes() }).ok()
}

/// Crockford's base32 alphabet, lowercase: no i, l, o or u, so nothing reads as another
/// character.
const RECOVERY_ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// A new recovery code: `xxxx-xxxx-xxxx-xxxx`, 16 characters of 5 random bits (80 bits).
pub fn new_recovery_code() -> String {
    let bytes = random_bytes::<16>();
    let chars: Vec<char> = bytes.iter().map(|b| RECOVERY_ALPHABET[(b & 31) as usize] as char).collect();
    chars.chunks(4).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join("-")
}

/// A recovery code as typed, normalized for comparison: lowercase, no spaces or dashes, and
/// the look-alikes Crockford's alphabet leaves out read as what they look like.
pub fn normalize_recovery_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_lowercase() {
            'o' => '0',
            'i' | 'l' => '1',
            c => c,
        })
        .collect()
}

/// What is stored for a recovery code of `account`.
pub fn recovery_hash(account: u64, code: &str) -> String {
    let mut hasher = blake3::Hasher::new_derive_key("bf2r master 2026-09 recovery codes v1");
    hasher.update(&account.to_le_bytes());
    hasher.update(normalize_recovery_code(code).as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// `text` as a QR code, drawn as an inline SVG (black modules on white, 4 modules of quiet
/// zone), `size` pixels wide.
pub fn qr_svg(text: &str, size: u32) -> Option<String> {
    let qr = qrcodegen::QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium).ok()?;
    let n = qr.size();
    let mut path = String::new();
    for y in 0..n {
        for x in 0..n {
            if qr.get_module(x, y) {
                path.push_str(&format!("M{},{}h1v1h-1z", x + 4, y + 4));
            }
        }
    }
    let view = n + 8;
    Some(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {view} {view}" width="{size}" height="{size}" shape-rendering="crispEdges" role="img" aria-label="QR code"><rect width="{view}" height="{view}" fill="#fff"/><path d="{path}" fill="#000"/></svg>"##
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_vectors() {
        // RFC 6238 appendix B, SHA-1 (the 8-digit codes' last 6 digits).
        let secret = b"12345678901234567890";
        for (time, code) in [
            (59u64, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
            (20_000_000_000, "353130"),
        ] {
            assert_eq!(code_at(secret, step_at(time)), code, "{time}");
        }
        // RFC 4226 appendix D (HOTP).
        assert_eq!(hotp(secret, 0), 755224);
        assert_eq!(hotp(secret, 9), 520489);
    }

    #[test]
    fn window_and_replay() {
        let secret = new_secret();
        let now = 1_800_000_000;
        let step = step_at(now);
        assert_eq!(verify(&secret, &code_at(&secret, step), now, 0), Some(step));
        assert_eq!(verify(&secret, &code_at(&secret, step - 1), now, 0), Some(step - 1), "one step late");
        assert_eq!(verify(&secret, &code_at(&secret, step + 1), now, 0), Some(step + 1), "one step early");
        assert_eq!(verify(&secret, &code_at(&secret, step + 3), now, 0), None, "too far off");
        // Replays: a step that was already used (or an earlier one) is refused.
        assert_eq!(verify(&secret, &code_at(&secret, step), now, step), None);
        assert_eq!(verify(&secret, &code_at(&secret, step - 1), now, step), None);
        assert_eq!(verify(&secret, &code_at(&secret, step + 1), now, step), Some(step + 1));
        let spaced = code_at(&secret, step);
        assert_eq!(verify(&secret, &format!("{} {}", &spaced[..3], &spaced[3..]), now, 0), Some(step));
        for bad in ["", "12345", "1234567", "abcdef", "12345x"] {
            assert_eq!(verify(&secret, bad, now, 0), None, "{bad}");
        }
        assert!(looks_like_code("123 456") && !looks_like_code("abcd-efgh"));
    }

    #[test]
    fn base32_round_trip() {
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32_encode(b"12345678901234567890"), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        let secret = new_secret();
        assert_eq!(base32_decode(&grouped(&base32_encode(&secret)).to_lowercase()).unwrap(), secret);
        assert!(base32_decode("not base32!").is_none());
        let uri = otpauth_uri("My master", "alice", "ABCD");
        assert_eq!(uri, "otpauth://totp/My%20master:alice?secret=ABCD&issuer=My%20master&algorithm=SHA1&digits=6&period=30");
        let svg = qr_svg(&uri, 200).unwrap();
        assert!(svg.starts_with("<svg") && svg.contains("<path d=\"M"));
    }

    #[test]
    fn sealing() {
        let key = game_auth::Identity::generate().derive_key(SEAL_CONTEXT);
        let secret = new_secret();
        let sealed = seal(&key, 7, &secret);
        assert!(!sealed.contains(&hex(&secret)));
        assert_eq!(open(&key, 7, &sealed).unwrap(), secret);
        assert!(open(&key, 8, &sealed).is_none(), "bound to the account");
        let other = game_auth::Identity::generate().derive_key(SEAL_CONTEXT);
        assert!(open(&other, 7, &sealed).is_none(), "bound to the master's key");
        let mut tampered = sealed.clone();
        tampered.replace_range(sealed.len() - 2.., if sealed.ends_with("00") { "11" } else { "00" });
        assert!(open(&key, 7, &tampered).is_none());
        assert!(open(&key, 7, "garbage").is_none());
    }

    #[test]
    fn recovery_codes() {
        let code = new_recovery_code();
        assert_eq!(code.len(), 19);
        assert_eq!(code.matches('-').count(), 3);
        assert_ne!(new_recovery_code(), code);
        assert_eq!(recovery_hash(1, &code), recovery_hash(1, &code.to_uppercase().replace('-', " ")));
        assert_eq!(normalize_recovery_code("O1L-i"), "0111");
        assert_ne!(recovery_hash(1, &code), recovery_hash(2, &code), "salted with the account");
    }
}
