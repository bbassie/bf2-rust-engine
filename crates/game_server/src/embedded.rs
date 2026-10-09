//! The server of singleplayer and hosting, on a thread of its own: the client starts it with
//! [`EmbeddedServer::start`] and plays through a [`ClientEnd`], an in-memory link that
//! carries Replicon's messages like a network connection would (`transport::LinkClient` on
//! this side). The server has a world of its own, so its game rules, bots and physics run
//! beside the client's frame instead of inside it, at the server's own pace (like the
//! dedicated server: [`TICK_HZ`] fixed ticks, updated twice a tick).
//!
//! Tools that need the server's world (scenario steps that move soldiers or seat bots, the
//! navigation debug view) send it closures ([`EmbeddedServer::run`], [`EmbeddedServer::query`]),
//! run on the server thread between two updates.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use bevy::{
    app::{PluginsState, ScheduleRunnerPlugin},
    ecs::schedule::{Schedules, SingleThreadedExecutor},
    prelude::*,
    state::app::StatesPlugin,
};
use bevy_replicon::{bytes::Bytes, prelude::*, shared::backend::connected_client::NetworkId};
pub use crossbeam_channel;
use crossbeam_channel::{Receiver, Sender};
use game_shared::{SharedPlugin, TICK_HZ, cache::Cache, config::GamePaths, protocol::Team};

use crate::{GameServerPlugin, ServerSettings, transport::{LinkClient, LinksOnly, ServerTransportPlugin}};

/// The network id of the client in this process (renet's are times in nanoseconds, never 1).
pub const LINK_CLIENT_ID: u64 = 1;

/// Work for the server's world, run on its thread between two updates.
pub type ServerTask = Box<dyn FnOnce(&mut World) + Send>;

/// The plugins every headless server app has, the dedicated server's and the embedded one's:
/// no window, no renderer (an explicit list rather than `DefaultPlugins`, so the server stays
/// headless even when a workspace build unifies rendering features into Bevy). Without a
/// runner and a log plugin: the dedicated server adds its own, the embedded one is driven by
/// its thread and logs through the client's.
pub fn add_headless_plugins(app: &mut App, paths: GamePaths, cache: Option<Cache>) {
    app.add_plugins((
        MinimalPlugins.build().disable::<ScheduleRunnerPlugin>(),
        StatesPlugin,
        TransformPlugin,
        AssetPlugin::default(),
    ))
    // Physics may expect mesh assets depending on unified features; they are CPU-only here.
    .init_asset::<Mesh>()
    .insert_resource(paths)
    .add_plugins((SharedPlugin, ServerTransportPlugin));
    if let Some(cache) = cache {
        app.insert_resource(cache);
    }
}

/// Runs every schedule on one thread: nearly every server system is tiny, and handing each
/// one to a thread cost more than running them in turn (Karkand, 32 bots: ticks 2.65 ->
/// 1.65 ms). Systems with real work split it themselves (`par_iter`).
pub fn single_threaded_schedules(app: &mut App) {
    let mut schedules = app.world_mut().resource_mut::<Schedules>();
    for (_, schedule) in schedules.iter_mut() {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
}

/// On the link client's entity: who plays through it.
#[derive(Component, Clone, Debug)]
pub struct LinkPlayer {
    /// `None`: watching without a soldier.
    pub name: Option<String>,
    pub team: Team,
}

/// The client's end of the link: Replicon messages, `(channel id, message)`.
pub struct ClientEnd {
    pub to_server: Sender<(usize, Bytes)>,
    pub from_server: Receiver<(usize, Bytes)>,
}

/// A server running on its own thread; stops when dropped.
pub struct EmbeddedServer {
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    tasks: Sender<ServerTask>,
}

impl EmbeddedServer {
    /// Starts serving `settings` on a new thread (listening on the network if
    /// `settings.network`), with a link client for the player of this process
    /// (`settings.local_player`, `None` to watch). Returns once the match started, or why it
    /// couldn't.
    pub fn start(mut settings: ServerSettings, paths: GamePaths, cache: Option<Cache>) -> Result<(Self, ClientEnd)> {
        let player = LinkPlayer {
            team: match &settings.local_player {
                None => Team::Spectator,
                Some(_) if crate::coop::is_coop(&settings.mode) => crate::coop::human_team(&settings),
                Some(_) if settings.local_team == 2 => Team::Two,
                Some(_) => Team::One,
            },
            name: settings.local_player.take(),
        };
        let (to_server, from_client) = crossbeam_channel::unbounded();
        let (to_client, from_server) = crossbeam_channel::unbounded();
        let (tasks, task_queue) = crossbeam_channel::unbounded::<ServerTask>();
        let (started_tx, started) = crossbeam_channel::bounded::<Result<()>>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::Builder::new()
            .name("Server".into())
            // Level loading and bots recurse deeper than the default 2 MiB allow.
            .stack_size(16 << 20)
            .spawn(move || {
                let mut app = App::new();
                add_headless_plugins(&mut app, paths, cache);
                app.add_plugins(GameServerPlugin { settings: None });
                single_threaded_schedules(&mut app);
                while app.plugins_state() == PluginsState::Adding {
                    bevy::tasks::tick_global_task_pools_on_main_thread();
                }
                app.finish();
                app.cleanup();
                let world = app.world_mut();
                if !settings.network {
                    world.insert_resource(LinksOnly);
                }
                if let Err(err) = crate::start_server(world, settings) {
                    let _ = started_tx.send(Err(anyhow!("{err}")));
                    return;
                }
                world.spawn((
                    ConnectedClient { max_size: LINK_MESSAGE_SIZE },
                    NetworkId::new(LINK_CLIENT_ID),
                    LinkClient { to_client, from_client },
                    player,
                    // Our own client needs no join handshake.
                    AuthorizedClient,
                ));
                let _ = started_tx.send(Ok(()));
                run(&mut app, &stopping, &task_queue);
                crate::stop_server(app.world_mut());
            })
            .context("can't start the server thread")?;
        let server = Self {
            thread: Some(thread),
            stop,
            tasks,
        };
        match started.recv() {
            Ok(Ok(())) => Ok((server, ClientEnd { to_server, from_server })),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(anyhow!("the server thread stopped while starting")),
        }
    }

    /// Runs `task` on the server's world before its next update.
    pub fn run(&self, task: impl FnOnce(&mut World) + Send + 'static) {
        let _ = self.tasks.send(Box::new(task));
    }

    /// Runs `task` on the server's world before its next update; the result arrives on the
    /// returned channel (within a frame or two).
    pub fn query<R: Send + 'static>(&self, task: impl FnOnce(&mut World) -> R + Send + 'static) -> Receiver<R> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.run(move |world| {
            let _ = tx.send(task(world));
        });
        rx
    }

    /// The thread ended (stopped, or panicked).
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for EmbeddedServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            error!("the server thread panicked");
        }
    }
}

/// Messages up to this size go over the link in one piece (a network packet is 1200 bytes;
/// splitting replication into packets only costs here).
const LINK_MESSAGE_SIZE: usize = 64 * 1024;

/// The server's loop: tasks, an update, then a wait until the next half tick.
fn run(app: &mut App, stop: &AtomicBool, tasks: &Receiver<ServerTask>) {
    let period = Duration::from_secs_f64(1.0 / (TICK_HZ * 2.0));
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        while let Ok(task) = tasks.try_recv() {
            task(app.world_mut());
        }
        app.update();
        if app.should_exit().is_some() {
            break;
        }
        if let Some(rest) = period.checked_sub(started.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

/// The player entity of the link client (the local human), in the server's world.
pub fn link_player(world: &mut World) -> Option<Entity> {
    let mut clients = world.query_filtered::<&crate::ClientPlayer, With<LinkClient>>();
    clients.iter(world).next().map(|c| c.0)
}

/// The soldier the link client's player controls, in the server's world.
pub fn link_soldier(world: &mut World) -> Option<Entity> {
    let player = link_player(world)?;
    world.get::<crate::Controls>(player).map(|c| c.0)
}
