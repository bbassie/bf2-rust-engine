//! Joining a server: the handshake that decides when a client is let into the match. The
//! server is in charge: it checks the client's game version, who the client is (an account
//! ticket, on servers that use accounts) and that the client has the server's content for
//! the level, and only then starts replicating to it (replicon's custom authorization).
//!
//! ```text
//! client                                      server
//!   connect (netcode)            ─────────▶   Joining: no replication, no player
//!   JoinRequest {protocol, nonce} ────────▶   protocol hash checked
//!                                ◀─────────   JoinChallenge {identity proof, level,
//!                                              content check, accounts}
//!   (checks the proof: signature of the nonce with the server's key)
//!   AccountTicket {ticket}        ────────▶   (servers with accounts) ticket checked offline
//!   ContentReport {digest}        ────────▶   digest compared with the server's manifest
//!                                ◀─────────   Accepted | SendHashes | Fetch {files} |
//!                                              ManifestChanged
//!   ContentReport {hashes}        ────────▶   (after SendHashes) per-file comparison
//!   (downloads what Fetch names over HTTP, then reports again)
//!                                ◀─────────   Accepted: replication starts, player created
//! ```
//!
//! On a map change the server sends every player a new [`JoinChallenge`] (`map_change`) and
//! nobody spawns until their report matches the new level. A client that doesn't match in
//! time, declines to download or offers no valid ticket to a ranked server is kicked with
//! the reason. See `game_server::join` and `game_client::join`, and
//! [`crate::content::verify`] for what is compared.

use bevy::prelude::*;
use bevy_replicon::prelude::ProtocolHash;
use game_auth::IdentityProof;
use serde::{Deserialize, Serialize};

use crate::content::{ContentMode, FileEntry};

/// Identity proofs made in the handshake use this purpose (see `game_auth::identity`).
pub const JOIN_PURPOSE: &str = "join";
/// Identity proofs of the content endpoint (`/content/identity`) use this one.
pub const CONTENT_PURPOSE: &str = "content";

/// Client -> server, right after connecting.
#[derive(Message, Serialize, Deserialize, Clone, Debug)]
pub struct JoinRequest {
    pub protocol: ProtocolHash,
    pub game_version: String,
    /// Random: the server signs it to prove it holds its key.
    pub nonce: [u8; 32],
}

/// Server -> a joining client, and to every player on a map change.
#[derive(Message, Serialize, Deserialize, Clone, Debug)]
pub struct JoinChallenge {
    pub server_name: String,
    /// The server's key and its signature of the request's nonce, the manifest id and the
    /// server name ([`JOIN_PURPOSE`]).
    pub identity: IdentityProof,
    pub level: String,
    /// What the client must have for `level`, if the server shares content. `None`: the
    /// server shares nothing, so nothing is checked.
    pub content: Option<ContentCheck>,
    /// The server knows a master server's key: it takes account tickets.
    pub accounts: Option<AccountsInfo>,
    /// For players already in the match: the map changed.
    pub map_change: bool,
    /// The server is still hashing its content: another challenge follows.
    pub preparing: bool,
    /// Seconds the client has to match before it is kicked.
    pub timeout_secs: u32,
}

impl JoinChallenge {
    /// The manifest id the identity proof covers (empty without content).
    pub fn manifest_id(&self) -> &str {
        self.content.as_ref().map_or("", |c| c.manifest_id.as_str())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ContentCheck {
    /// [`crate::content::Manifest::content_id`] of the server's manifest.
    pub manifest_id: String,
    pub mode: ContentMode,
    /// Files and bytes the level requires ([`crate::content::Manifest::required`]).
    pub files: u32,
    pub bytes: u64,
    /// TCP port of the content endpoint (HTTP).
    pub port: u16,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AccountsInfo {
    /// Fingerprint of the master server key the server trusts.
    pub master_fingerprint: String,
    /// Where accounts are made (the master's web address), for messages.
    pub master_url: String,
    /// A ranked server: players need an account.
    pub required: bool,
}

/// Client -> server, when the challenge offers accounts: a ticket for this server from the
/// master server, or none (not logged in, or another master).
#[derive(Message, Serialize, Deserialize, Clone, Debug)]
pub struct AccountTicket {
    pub ticket: Option<String>,
}

/// Client -> server: what it has for the level.
#[derive(Message, Serialize, Deserialize, Clone, Debug, Default)]
pub struct ContentReport {
    pub level: String,
    pub manifest_id: String,
    /// [`crate::content::verify::digest`] of the client's files for the required list.
    pub digest: String,
    /// The client's hash of each required file, in the required list's order (zeros for a
    /// missing file). Only after [`JoinVerdict::SendHashes`].
    pub hashes: Vec<[u8; 32]>,
    /// The client won't download what the server asked for: why.
    pub declined: Option<String>,
}

/// Server -> client: what the report showed.
#[derive(Message, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum JoinVerdict {
    /// In (a join: replication starts; a map change: spawning is allowed again).
    Accepted,
    /// The digest differs: send the per-file hashes.
    SendHashes,
    /// The server's manifest changed since the client got it: fetch it again and report.
    ManifestChanged { manifest_id: String },
    /// These of the server's files differ from the client's: download them by hash and report
    /// again. `more`: further differing files not listed (fetch the manifest's plan).
    Fetch { files: Vec<FetchFile>, more: u32 },
}

/// A file to download again (a [`FileEntry`] without its levels; `FileEntry` skips empty
/// fields, which the binary wire format can't).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FetchFile {
    pub path: String,
    pub size: u64,
    pub hash: String,
}

impl From<&FileEntry> for FetchFile {
    fn from(entry: &FileEntry) -> Self {
        Self { path: entry.path.clone(), size: entry.size, hash: entry.hash.clone() }
    }
}

impl From<FetchFile> for FileEntry {
    fn from(file: FetchFile) -> Self {
        Self { path: file.path, size: file.size, hash: file.hash, levels: Vec::new() }
    }
}

/// A player's verified account (replicated): name and rank shown on the scoreboard.
#[derive(Component, Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct AccountBadge {
    pub rank: u32,
    pub rank_name: String,
    /// `Sgt`.
    pub rank_short: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Through the binary format replicon sends messages in, which has no field names: a
    /// skipped field breaks everything after it.
    fn round_trip<T: Serialize + serde::de::DeserializeOwned>(value: &T) -> T {
        postcard::from_bytes(&postcard::to_allocvec(value).unwrap()).unwrap()
    }

    #[test]
    fn messages_survive_the_wire() {
        let entry = FileEntry { path: "kits/x.ron".into(), size: 5, hash: "ab".repeat(32), levels: vec![] };
        let fetch = JoinVerdict::Fetch { files: vec![(&entry).into(), (&FileEntry { levels: vec![1, 2], ..entry.clone() }).into()], more: 3 };
        assert_eq!(round_trip(&fetch), fetch);
        for verdict in [JoinVerdict::Accepted, JoinVerdict::SendHashes, JoinVerdict::ManifestChanged { manifest_id: "id".into() }] {
            assert_eq!(round_trip(&verdict), verdict);
        }
        let challenge = JoinChallenge {
            server_name: "Server".into(),
            identity: IdentityProof { public_key: "k".into(), signature: "s".into() },
            level: "sample_valley".into(),
            content: Some(ContentCheck { manifest_id: "m".into(), mode: ContentMode::All, files: 3, bytes: 9, port: 16567 }),
            accounts: Some(AccountsInfo { master_fingerprint: "f".into(), master_url: "https://m".into(), required: true }),
            map_change: true,
            preparing: false,
            timeout_secs: 600,
        };
        let back = round_trip(&challenge);
        assert_eq!((back.level, back.content, back.accounts, back.timeout_secs), (challenge.level, challenge.content, challenge.accounts, 600));
        let report = ContentReport { level: "l".into(), manifest_id: "m".into(), digest: "d".into(), hashes: vec![[7; 32]], declined: Some("no".into()) };
        let back = round_trip(&report);
        assert_eq!((back.hashes, back.declined), (report.hashes, report.declined));
        assert_eq!(round_trip(&AccountTicket { ticket: Some("t".into()) }).ticket.as_deref(), Some("t"));
        let badge = AccountBadge { rank: 4, rank_name: "Sergeant".into(), rank_short: "Sgt".into() };
        assert_eq!(round_trip(&badge), badge);
    }
}
