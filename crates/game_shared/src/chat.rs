//! Text chat like BF2's: everyone, team and squad channels, relayed by the server, plus
//! server notices (joins, map changes, the message of the day, admin replies).

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::protocol::Team;

/// Longest chat line in characters.
pub const MAX_CHAT_LENGTH: usize = 120;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChatChannel {
    All,
    Team,
    Squad,
    /// From the server: joins, leaves, map changes, admin announcements.
    Server,
    /// From the server to one player only: admin replies, the message of the day.
    Private,
}

/// Client -> server: a line typed into the chat box. Lines starting with `/` or `!` are
/// admin commands.
#[derive(Message, Serialize, Deserialize, Clone, Debug)]
pub struct ChatRequest {
    pub channel: ChatChannel,
    pub text: String,
}

/// Server -> clients: a chat line to show.
#[derive(Message, Serialize, Deserialize, Clone, Debug)]
pub struct ChatLine {
    pub channel: ChatChannel,
    /// Who said it; `None` for server messages.
    pub sender: Option<String>,
    /// The sender's team, for its color.
    pub team: Team,
    pub text: String,
}

impl ChatLine {
    pub fn server(text: impl Into<String>) -> Self {
        Self {
            channel: ChatChannel::Server,
            sender: None,
            team: Team::Spectator,
            text: text.into(),
        }
    }

    pub fn private(text: impl Into<String>) -> Self {
        Self {
            channel: ChatChannel::Private,
            ..Self::server(text)
        }
    }
}

/// Server -> one client, right before it is disconnected: why.
#[derive(Message, Serialize, Deserialize, Clone, Debug)]
pub struct Kicked {
    pub reason: String,
}

/// Printable ASCII only (the UI font has nothing else), without leading or trailing
/// spaces, at most `max` characters.
pub fn clean_text(text: &str, max: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' })
        .take(max)
        .collect();
    cleaned.trim().to_string()
}
