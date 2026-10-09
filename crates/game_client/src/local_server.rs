//! Singleplayer and hosting: the server runs on a thread of its own with its own world
//! (`game_server::embedded`), and we play on it like on any server, through an in-memory
//! link instead of UDP. Its game rules, bots and physics then cost the frame nothing: they
//! run beside it, on another core.
//!
//! The link is our Replicon messaging backend while [`LocalServer`] exists (renet's is while
//! `RenetClient` does): it moves the connection state along and carries the messages.
//! Scenario steps and debug views that need the server's world send it closures
//! ([`LocalServer::run`], [`LocalServer::query`]).

use bevy::prelude::*;
use bevy_replicon::prelude::*;
use game_server::{
    ServerSettings,
    embedded::{ClientEnd, EmbeddedServer, crossbeam_channel::Receiver},
};
use game_shared::{cache::Cache, config::GamePaths, protocol::ClientHello};

use crate::net::{self, MatchNotice};

pub struct LocalServerPlugin;

impl Plugin for LocalServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PreUpdate, (update_state, receive).chain().in_set(ClientSystems::ReceivePackets))
            .add_systems(PostUpdate, send.in_set(ClientSystems::SendPackets))
            .add_systems(OnEnter(ClientState::Connected), say_hello)
            .add_systems(Update, (pause_with_us, watch_thread).run_if(resource_exists::<LocalServer>));
    }
}

/// The server of this process and our link to it.
#[derive(Resource)]
pub struct LocalServer {
    server: EmbeddedServer,
    link: ClientEnd,
    /// Our player's name, `None` watching.
    name: Option<String>,
    /// Whether the server's clock is paused with ours (singleplayer's Esc menu).
    paused: bool,
}

impl LocalServer {
    /// Runs `task` on the server's world before its next update.
    pub fn run(&self, task: impl FnOnce(&mut World) + Send + 'static) {
        self.server.run(task);
    }

    /// Runs `task` on the server's world; the result arrives on the returned channel within
    /// a frame or two.
    pub fn query<R: Send + 'static>(&self, task: impl FnOnce(&mut World) -> R + Send + 'static) -> Receiver<R> {
        self.server.query(task)
    }
}

/// Starts the server of `settings` on its thread and links us to it.
pub fn start(world: &mut World, settings: ServerSettings) -> Result<()> {
    let paths = world.resource::<GamePaths>().clone();
    let cache = world.get_resource::<Cache>().cloned();
    let name = settings.local_player.clone();
    let (server, link) = EmbeddedServer::start(settings, paths, cache)?;
    world.insert_resource(LocalServer {
        server,
        link,
        name,
        paused: false,
    });
    Ok(())
}

/// Connecting as soon as the link exists, connected a frame later (like renet's backend:
/// through `Connecting`, so everything waiting for the transition sees it); disconnected
/// once it is gone.
fn update_state(
    server: Option<Res<LocalServer>>,
    state: Res<State<ClientState>>,
    mut next: ResMut<NextState<ClientState>>,
    mut linked: Local<bool>,
) {
    match (server.is_some(), *state.get()) {
        (true, ClientState::Disconnected) => next.set(ClientState::Connecting),
        (true, ClientState::Connecting) => next.set(ClientState::Connected),
        (false, ClientState::Connected | ClientState::Connecting) if *linked => next.set(ClientState::Disconnected),
        _ => {}
    }
    *linked = server.is_some();
}

fn receive(server: Option<Res<LocalServer>>, state: Res<State<ClientState>>, mut messages: ResMut<ClientMessages>) {
    let Some(server) = server else {
        return;
    };
    if *state.get() != ClientState::Connected {
        return;
    }
    for (channel_id, message) in server.link.from_server.try_iter() {
        messages.insert_received(channel_id, message);
    }
}

fn send(server: Option<Res<LocalServer>>, state: Res<State<ClientState>>, mut messages: ResMut<ClientMessages>) {
    let Some(server) = server else {
        return;
    };
    if *state.get() != ClientState::Connected {
        return;
    }
    for (channel_id, message) in messages.drain_sent() {
        let _ = server.link.to_server.send((channel_id, message));
    }
}

/// Our name, as a joining client says it (the server let us in without a handshake).
fn say_hello(server: Option<Res<LocalServer>>, mut hellos: MessageWriter<ClientHello>) {
    if let Some(name) = server.and_then(|s| s.name.clone()) {
        hellos.write(ClientHello { name });
    }
}

/// Singleplayer's Esc menu pauses our clock (`menu::loading::pause_time`): the server's too.
fn pause_with_us(time: Res<Time<Virtual>>, mut server: ResMut<LocalServer>) {
    let paused = time.is_paused();
    if server.paused != paused {
        server.paused = paused;
        server.run(move |world| {
            let mut time = world.resource_mut::<Time<Virtual>>();
            if paused {
                time.pause();
            } else {
                time.unpause();
            }
        });
    }
}

/// The server thread ended on its own (it panicked): back to the menu, saying so.
fn watch_thread(server: Res<LocalServer>, mut commands: Commands) {
    if server.server.is_finished() {
        commands.queue(|world: &mut World| {
            let notice = "The server stopped unexpectedly (see the log).".to_string();
            error!("{notice}");
            net::leave_match(world);
            world.insert_resource(MatchNotice(Some(notice)));
        });
    }
}
