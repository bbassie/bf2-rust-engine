//! Ed25519 identity keys: a game server's (generated on its first start, kept in its data
//! folder) and the master server's (which signs account tokens).
//!
//! A server proves it holds its key by signing a client's random challenge
//! ([`IdentityProof`]), so a server can't pose as another one by copying its public key.
//! Players compare [`fingerprint`]s: the first 128 bits of the public key's BLAKE3 hash.

use std::{
    io,
    path::Path,
};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::{hex, random_bytes, unhex_array};

/// First line of a key file.
const KEY_FILE_HEADER: &str = "bf2r-ed25519-secret-key 1";

/// A private key.
pub struct Identity {
    key: SigningKey,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the secret.
        write!(f, "Identity({})", self.fingerprint())
    }
}

impl Identity {
    pub fn generate() -> Self {
        Self::from_secret(random_bytes())
    }

    pub fn from_secret(secret: [u8; 32]) -> Self {
        Self { key: SigningKey::from_bytes(&secret) }
    }

    /// Reads a key file.
    pub fn load(path: &Path) -> io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut lines = text.lines();
        let bad = || io::Error::new(io::ErrorKind::InvalidData, format!("{}: not a key file", path.display()));
        if lines.next().map(str::trim) != Some(KEY_FILE_HEADER) {
            return Err(bad());
        }
        let secret = lines.next().and_then(unhex_array::<32>).ok_or_else(bad)?;
        Ok(Self::from_secret(secret))
    }

    /// Writes the key file, readable by this user only: a file mode on Unix, a best-effort
    /// ACL restriction on Windows (S38, below), since Windows has no equivalent of the mode
    /// bits and the containing folder isn't always the private, per-user default (`--data-dir`
    /// / `--identity` can point anywhere, including a folder other accounts can read).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temp = path.with_extension(format!("tmp{}", std::process::id()));
        let text = format!("{KEY_FILE_HEADER}\n{}\n", hex(self.key.as_bytes()));
        {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            io::Write::write_all(&mut file, text.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&temp, path)?;
        #[cfg(windows)]
        restrict_windows_acl(path);
        Ok(())
    }

    /// The key in `path`, or a new one saved there. Also says whether it is new.
    pub fn load_or_create(path: &Path) -> io::Result<(Self, bool)> {
        match Self::load(path) {
            Ok(identity) => Ok((identity, false)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let identity = Self::generate();
                identity.save(path)?;
                Ok((identity, true))
            }
            Err(err) => Err(err),
        }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    pub fn public_hex(&self) -> String {
        hex(&self.public_key())
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public_key())
    }

    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.key.sign(message).to_bytes()
    }

    /// Proves this identity to whoever sent `nonce` (see [`proof_message`]).
    pub fn prove(&self, purpose: &str, nonce: &[u8; 32], manifest_id: &str, name: &str) -> IdentityProof {
        IdentityProof {
            public_key: self.public_hex(),
            signature: hex(&self.sign(&proof_message(purpose, nonce, manifest_id, name))),
        }
    }
}

/// Restricts a just-written key file to the current user only, using the OS's own `icacls`
/// (no extra dependency for this). Best-effort: `icacls` can be missing, the volume might not
/// be NTFS, or the environment might be locked down; any of that only warns (S38); the key is
/// written either way, just not necessarily locked down beyond the containing folder.
#[cfg(windows)]
fn restrict_windows_acl(path: &Path) {
    let user = std::env::var("USERNAME").unwrap_or_default();
    let account = match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() && !user.is_empty() => format!("{domain}\\{user}"),
        _ => user.clone(),
    };
    let restricted = !account.is_empty()
        && std::process::Command::new("icacls")
            .arg(path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(format!("{account}:F"))
            .output()
            .is_ok_and(|o| o.status.success());
    if !restricted {
        eprintln!(
            "warning: couldn't restrict {}'s permissions to the current user (is `icacls` available?); keep \
             its folder private, since anyone who can read this file can impersonate its holder",
            path.display()
        );
    }
}

/// `3f2a 91bc 04de 77e1 a0b2 c3d4 e5f6 0718`: the first 16 bytes of the key's BLAKE3 hash.
pub fn fingerprint(public_key: &[u8; 32]) -> String {
    let hash = blake3::hash(public_key);
    hash.as_bytes()[..16]
        .chunks(2)
        .map(hex)
        .collect::<Vec<_>>()
        .join(" ")
}

/// A fingerprint as typed or shown, without spaces and lowercase, for comparisons.
pub fn normalize_fingerprint(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace() && *c != ':' && *c != '-').collect::<String>().to_ascii_lowercase()
}

/// Whether `signature` is `public_key`'s signature of `message`.
pub fn verify(public_key: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(public_key) else {
        return false;
    };
    key.verify_strict(message, &Signature::from_bytes(signature)).is_ok()
}

/// What a server signs to prove its identity: a purpose (so a signature made for one use
/// can't be replayed in another), the client's random nonce, the server's content manifest
/// id (empty without) and its name.
pub fn proof_message(purpose: &str, nonce: &[u8; 32], manifest_id: &str, name: &str) -> Vec<u8> {
    format!("BF2R-IDENTITY-1\n{purpose}\n{}\n{manifest_id}\n{name}", hex(nonce)).into_bytes()
}

/// A server's public key and its signature of a [`proof_message`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct IdentityProof {
    /// 64 hex digits.
    pub public_key: String,
    /// 128 hex digits.
    pub signature: String,
}

impl IdentityProof {
    /// Checks the signature; returns the public key.
    pub fn check(&self, purpose: &str, nonce: &[u8; 32], manifest_id: &str, name: &str) -> Result<[u8; 32], String> {
        let key = unhex_array::<32>(&self.public_key).ok_or("the server's key is malformed")?;
        let signature = unhex_array::<64>(&self.signature).ok_or("the server's signature is malformed")?;
        if !verify(&key, &proof_message(purpose, nonce, manifest_id, name), &signature) {
            return Err("the server couldn't prove its identity (bad signature)".into());
        }
        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_sign_and_persist() {
        let dir = std::env::temp_dir().join(format!("bf2_identity_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("identity.key");
        let (first, created) = Identity::load_or_create(&path).unwrap();
        assert!(created);
        let (again, created) = Identity::load_or_create(&path).unwrap();
        assert!(!created);
        assert_eq!(first.public_key(), again.public_key());
        assert_eq!(first.fingerprint().len(), 39);
        assert!(!format!("{first:?}").contains(&hex(first.key.as_bytes())));

        let nonce = random_bytes();
        let proof = first.prove("join", &nonce, "abc", "Server");
        assert_eq!(proof.check("join", &nonce, "abc", "Server").unwrap(), first.public_key());
        // Another nonce, purpose, manifest or name: refused.
        assert!(proof.check("join", &random_bytes(), "abc", "Server").is_err());
        assert!(proof.check("content", &nonce, "abc", "Server").is_err());
        assert!(proof.check("join", &nonce, "abd", "Server").is_err());
        assert!(proof.check("join", &nonce, "abc", "Other").is_err());
        // Someone else's key with this signature: refused.
        let other = Identity::generate();
        let forged = IdentityProof { public_key: other.public_hex(), ..proof.clone() };
        assert!(forged.check("join", &nonce, "abc", "Server").is_err());
        assert_ne!(first.fingerprint(), other.fingerprint());
        assert_eq!(normalize_fingerprint(&first.fingerprint()).len(), 32);

        std::fs::write(&path, "garbage").unwrap();
        assert!(Identity::load(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
