//! Account tokens: signed by the master server, checked offline by anyone with its public
//! key. `base64url(JSON claims) "." base64url(Ed25519 signature)`, like a JWT without the
//! header (there is one algorithm and one version).
//!
//! - A **session** token (minutes) lets the client use the master's API.
//! - A **ticket** (a few minutes) is for joining one game server: its audience is that
//!   server's key fingerprint, so a server that receives a ticket can't use it elsewhere.
//!   Servers also remember the tickets they accepted until they expire, so one can't be
//!   replayed there either.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

use crate::{Identity, identity::verify};

pub const TOKEN_VERSION: u32 = 1;
/// Longest token accepted.
pub const MAX_TOKEN_LEN: usize = 2048;
/// Clocks differ between machines: tokens issued a little "in the future" are fine, and
/// expired ones count a little longer.
pub const CLOCK_LEEWAY_SECS: u64 = 120;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TokenKind {
    Session,
    Ticket,
}

/// What a token says.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Claims {
    /// [`TOKEN_VERSION`].
    pub v: u32,
    pub kind: TokenKind,
    /// Account id.
    pub sub: u64,
    /// Account name.
    pub name: String,
    /// Rank index and name when the token was issued.
    pub rank: u32,
    pub rank_name: String,
    /// The rank's abbreviation (`Sgt`), for scoreboards.
    #[serde(default)]
    pub rank_short: String,
    /// Issued and expires, seconds since 1970.
    pub iat: u64,
    pub exp: u64,
    /// Tickets: the fingerprint of the game server it is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aud: Option<String>,
    /// Random id (hex).
    pub jti: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenError {
    Malformed,
    BadSignature,
    Expired,
    NotYetValid,
    WrongKind,
    WrongAudience,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TokenError::Malformed => "malformed token",
            TokenError::BadSignature => "token not signed by the master server",
            TokenError::Expired => "token expired",
            TokenError::NotYetValid => "token not valid yet (check the clock)",
            TokenError::WrongKind => "wrong kind of token",
            TokenError::WrongAudience => "token is for another server",
        })
    }
}

impl std::error::Error for TokenError {}

fn signing_input(payload: &str) -> Vec<u8> {
    [b"BF2R-TOKEN-1.".as_slice(), payload.as_bytes()].concat()
}

/// Signs `claims` with the master's key.
pub fn sign(master: &Identity, claims: &Claims) -> String {
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("claims serialize"));
    let signature = master.sign(&signing_input(&payload));
    format!("{payload}.{}", URL_SAFE_NO_PAD.encode(signature))
}

/// The claims of a token, without checking anything: for showing what a stored token says.
pub fn peek(token: &str) -> Option<Claims> {
    let (payload, _) = token.split_once('.')?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
}

/// Checks a token's signature, version, kind, lifetime and (for tickets) audience.
pub fn verify_token(
    master_key: &[u8; 32],
    token: &str,
    now: u64,
    kind: TokenKind,
    audience: Option<&str>,
) -> Result<Claims, TokenError> {
    if token.len() > MAX_TOKEN_LEN {
        return Err(TokenError::Malformed);
    }
    let (payload, signature) = token.trim().split_once('.').ok_or(TokenError::Malformed)?;
    let signature: [u8; 64] = URL_SAFE_NO_PAD
        .decode(signature)
        .ok()
        .and_then(|s| s.try_into().ok())
        .ok_or(TokenError::Malformed)?;
    if !verify(master_key, &signing_input(payload), &signature) {
        return Err(TokenError::BadSignature);
    }
    let claims: Claims = URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .and_then(|json| serde_json::from_slice(&json).ok())
        .ok_or(TokenError::Malformed)?;
    if claims.v != TOKEN_VERSION {
        return Err(TokenError::Malformed);
    }
    if claims.kind != kind {
        return Err(TokenError::WrongKind);
    }
    if claims.iat > now + CLOCK_LEEWAY_SECS {
        return Err(TokenError::NotYetValid);
    }
    if now > claims.exp + CLOCK_LEEWAY_SECS {
        return Err(TokenError::Expired);
    }
    if let Some(audience) = audience {
        let wanted = crate::identity::normalize_fingerprint(audience);
        if claims.aud.as_deref().map(crate::identity::normalize_fingerprint) != Some(wanted) {
            return Err(TokenError::WrongAudience);
        }
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(kind: TokenKind, now: u64, aud: Option<&str>) -> Claims {
        Claims {
            v: TOKEN_VERSION,
            kind,
            sub: 7,
            name: "alice".into(),
            rank: 3,
            rank_name: "Corporal".into(),
            rank_short: "Cpl".into(),
            iat: now,
            exp: now + 300,
            aud: aud.map(str::to_string),
            jti: "0123456789abcdef".into(),
        }
    }

    #[test]
    fn sign_and_verify() {
        let master = Identity::generate();
        let key = master.public_key();
        let now = 1_700_000_000;
        let server = Identity::generate().fingerprint();
        let ticket = sign(&master, &claims(TokenKind::Ticket, now, Some(&server)));
        let got = verify_token(&key, &ticket, now + 10, TokenKind::Ticket, Some(&server)).unwrap();
        assert_eq!(got.name, "alice");
        assert_eq!(peek(&ticket).unwrap(), got);
        // The fingerprint compares without spaces and case.
        assert!(verify_token(&key, &ticket, now, TokenKind::Ticket, Some(&server.replace(' ', "").to_uppercase())).is_ok());
        assert_eq!(
            verify_token(&key, &ticket, now, TokenKind::Ticket, Some(&Identity::generate().fingerprint())),
            Err(TokenError::WrongAudience)
        );
        assert_eq!(verify_token(&key, &ticket, now, TokenKind::Session, None), Err(TokenError::WrongKind));
        assert_eq!(verify_token(&key, &ticket, now + 300 + CLOCK_LEEWAY_SECS + 1, TokenKind::Ticket, None), Err(TokenError::Expired));
        assert_eq!(verify_token(&key, &ticket, now - CLOCK_LEEWAY_SECS - 1, TokenKind::Ticket, None), Err(TokenError::NotYetValid));
        // Another master's key, or a changed payload: refused.
        let other = Identity::generate().public_key();
        assert_eq!(verify_token(&other, &ticket, now, TokenKind::Ticket, None), Err(TokenError::BadSignature));
        let (payload, signature) = ticket.split_once('.').unwrap();
        let mut forged = claims(TokenKind::Ticket, now, Some(&server));
        forged.name = "mallory".into();
        let forged_payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).unwrap());
        assert_ne!(forged_payload, payload);
        assert_eq!(
            verify_token(&key, &format!("{forged_payload}.{signature}"), now, TokenKind::Ticket, None),
            Err(TokenError::BadSignature)
        );
        for garbage in ["", ".", "abc", "a.b", &"x".repeat(MAX_TOKEN_LEN + 1)] {
            assert!(verify_token(&key, garbage, now, TokenKind::Ticket, None).is_err(), "{garbage}");
        }
    }
}
