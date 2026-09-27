//! BF2's commander: one player per team who orders squads around and calls in the team's
//! assets: artillery, the UAV, satellite scans and supply drops.
//!
//! Players apply for the post, resign, or vote their commander out (mutiny) with
//! [`CommanderRequest`]s; the server (`game_server::commander`) marks the commander with
//! [`Commander`], keeps each squad's [`SquadOrder`], each team's [`TeamAssets`] and what is
//! going on ([`AssetEffect`]: strikes, UAVs, supply crates), all replicated. The assets
//! themselves are objects of the level's layout ([`layout_assets`]), spawned on the server
//! and every client as destroyable objects numbered from [`ASSET_INSTANCE_BASE`]: while
//! destroyed they can't be used, and they come back after [`ASSET_RESPAWN_SECONDS`]. The
//! artillery pieces are vehicles instead (tagged [`AssetVehicle`]): a strike is fired by the
//! team's living pieces, and a destroyed piece stays a wreck until it is repaired or comes
//! back.

use std::path::Path;

use bevy::prelude::*;
use game_data::{AssetKind, CommanderDesc, Placement, StaticInstance};
use serde::{Deserialize, Serialize};

use crate::{
    config::GamePaths,
    level::LoadedLevel,
    protocol::{MatchInfo, Team},
    statics::spawn_objects,
};

pub struct CommanderPlugin;

impl Plugin for CommanderPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CommanderAssets>().add_systems(
            Update,
            spawn_assets.run_if(resource_exists_and_changed::<LoadedLevel>),
        );
    }
}

/// Seconds until an asset can be used again [inferred: BF2 keeps these in the engine;
/// these are the commonly quoted values].
pub const ARTILLERY_RECHARGE: f32 = 180.0;
pub const UAV_RECHARGE: f32 = 150.0;
pub const SCAN_RECHARGE: f32 = 60.0;
pub const SUPPLY_RECHARGE: f32 = 120.0;
/// How long a UAV circles and how long a scan shows the enemy on the commander's map
/// (seconds) [inferred].
pub const UAV_SECONDS: f32 = 60.0;
pub const SCAN_SECONDS: f32 = 6.0;
/// Seconds from calling in artillery until the first shells land [inferred: the flight of
/// shells fired at 450 m/s from the team's guns].
pub const ARTILLERY_DELAY: f32 = 5.0;
/// Seconds between the first shells of successive guns of a strike.
pub const ARTILLERY_GUN_STAGGER: f32 = 0.4;
/// A destroyed asset comes back after this long, like the layouts' spawners
/// (`ObjectSpawner.minSpawnDelay 360`).
pub const ASSET_RESPAWN_SECONDS: f32 = 360.0;
/// [`crate::statics::DestroyedStatics`] numbers of asset objects start here, above any
/// level's statics.
pub const ASSET_INSTANCE_BASE: u32 = 1 << 24;
/// Share of a team's players whose votes remove its commander (BF2's mutiny vote needs a
/// majority).
pub const MUTINY_SHARE: f32 = 0.5;

/// The team's commander. On the player. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Commander;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OrderKind {
    Attack,
    Defend,
    Move,
}

impl OrderKind {
    pub const ALL: [OrderKind; 3] = [OrderKind::Attack, OrderKind::Defend, OrderKind::Move];

    pub fn label(self) -> &'static str {
        match self {
            OrderKind::Attack => "Attack",
            OrderKind::Defend => "Defend",
            OrderKind::Move => "Move",
        }
    }

    /// BF2's radio message id.
    pub fn message_id(self) -> &'static str {
        match self {
            OrderKind::Attack => "attack_position",
            OrderKind::Defend => "defend",
            OrderKind::Move => "move",
        }
    }
}

/// What the commander calls in.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Asset {
    Artillery,
    Uav,
    Scan,
    Supply,
}

impl Asset {
    pub const ALL: [Asset; 4] = [Asset::Artillery, Asset::Uav, Asset::Scan, Asset::Supply];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn label(self) -> &'static str {
        match self {
            Asset::Artillery => "Artillery",
            Asset::Uav => "UAV",
            Asset::Scan => "Satellite scan",
            Asset::Supply => "Supply drop",
        }
    }

    /// The level object it needs; supply drops need none.
    pub fn object(self) -> Option<AssetKind> {
        match self {
            Asset::Artillery => Some(AssetKind::Artillery),
            Asset::Uav => Some(AssetKind::Uav),
            Asset::Scan => Some(AssetKind::Radar),
            Asset::Supply => None,
        }
    }

    pub fn recharge(self) -> f32 {
        match self {
            Asset::Artillery => ARTILLERY_RECHARGE,
            Asset::Uav => UAV_RECHARGE,
            Asset::Scan => SCAN_RECHARGE,
            Asset::Supply => SUPPLY_RECHARGE,
        }
    }

    /// Whether it goes to a point on the map.
    pub fn targeted(self) -> bool {
        self != Asset::Scan
    }
}

/// Client -> server.
#[derive(Message, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum CommanderRequest {
    /// Take the team's vacant commander post.
    Apply,
    Resign,
    /// Vote the team's commander out.
    Mutiny,
    /// Commander: order a squad (1-based).
    Order { squad: u8, kind: OrderKind, target: Vec3 },
    CancelOrder { squad: u8 },
    /// Commander: call in an asset (scans ignore the target).
    Use { asset: Asset, target: Vec3 },
    /// Commander: spot the enemy nearest to this point on the map for the team (what his
    /// satellite scan shows him, like BF2's commander map).
    Spot { target: Vec3 },
}

/// Server -> the commander, every second of his satellite scan: where every enemy is. Like
/// BF2, the scan shows them on the commander's map only; he spots them for his team.
#[derive(Message, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ScanReport {
    pub contacts: Vec<ScanContact>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct ScanContact {
    pub position: Vec3,
    /// In a vehicle (the vehicle's position).
    pub vehicle: bool,
}

/// A squad's order. Replicated, one entity per ordered squad.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct SquadOrder {
    pub team: Team,
    pub squad: u8,
    pub kind: OrderKind,
    pub position: Vec3,
}

/// Whether an asset can be used, and when.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AssetStatus {
    /// Its level object stands (always for supply drops).
    pub intact: bool,
    /// Whole seconds until it is recharged.
    pub recharge: u16,
}

impl AssetStatus {
    pub fn ready(&self) -> bool {
        self.intact && self.recharge == 0
    }
}

/// A team's assets, by [`Asset::index`]. Replicated, one entity per team.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct TeamAssets {
    pub team: Team,
    pub status: [AssetStatus; 4],
}

impl TeamAssets {
    pub fn get(&self, asset: Asset) -> AssetStatus {
        self.status[asset.index()]
    }
}

/// Something a commander called in, while it lasts: an artillery strike, a UAV circling, a
/// supply crate (falling while `position` is above the ground). Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct AssetEffect {
    pub team: Team,
    pub asset: Asset,
    pub position: Vec3,
    pub radius: f32,
}

/// An asset object of the loaded layout.
#[derive(Clone, Debug)]
pub struct AssetInstance {
    pub kind: AssetKind,
    pub team: Team,
    pub template: String,
    pub placement: Placement,
    /// Its [`crate::statics::DestroyedStatics`] number (in the set while it is destroyed,
    /// also when it is a vehicle).
    pub instance: u32,
    /// It is a vehicle (`vehicles/<template>.ron`, the commander's artillery): the layout's
    /// vehicle spawner puts it there (tagged with [`AssetVehicle`]) instead of it being a
    /// destroyable object.
    pub vehicle: bool,
}

/// A vehicle that is one of the commander's assets (the artillery): which one of
/// [`CommanderAssets::instances`]. On the vehicle. Replicated.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct AssetVehicle {
    pub kind: AssetKind,
    pub team: Team,
    pub instance: u32,
}

/// The loaded level's commander settings and the layout's assets.
#[derive(Resource, Default)]
pub struct CommanderAssets {
    pub desc: CommanderDesc,
    pub instances: Vec<AssetInstance>,
}

impl CommanderAssets {
    /// A team's objects of a kind.
    pub fn of(&self, team: Team, kind: AssetKind) -> impl Iterator<Item = &AssetInstance> {
        self.instances.iter().filter(move |a| a.team == team && a.kind == kind)
    }
}

/// The asset objects of a layout: its vehicle spawners whose template is an asset.
pub fn layout_assets(level: &LoadedLevel, info: &MatchInfo, desc: &CommanderDesc) -> Vec<AssetInstance> {
    let Some(layout) = level.game_mode(&info.mode, info.size) else {
        return Vec::new();
    };
    let mut assets = Vec::new();
    for spawner in &layout.vehicle_spawners {
        for (index, template) in spawner.templates.iter().enumerate() {
            let Some((template, kind)) = template.as_ref().and_then(|t| desc.assets.get(t).map(|k| (t, *k))) else {
                continue;
            };
            assets.push(AssetInstance {
                kind,
                team: if index == 0 { Team::One } else { Team::Two },
                template: template.clone(),
                placement: spawner.placement,
                instance: ASSET_INSTANCE_BASE + assets.len() as u32,
                vehicle: false,
            });
        }
    }
    assets
}

fn load_desc(dir: &Path) -> CommanderDesc {
    let path = dir.join("commander.ron");
    if !path.exists() {
        return CommanderDesc::default();
    }
    game_data::read_ron(&path).unwrap_or_else(|err| {
        warn!("{err}");
        CommanderDesc::default()
    })
}

/// The layout's asset objects, on the server and on every client alike.
fn spawn_assets(
    mut commands: Commands,
    level: Res<LoadedLevel>,
    info: Query<&MatchInfo>,
    paths: Res<GamePaths>,
    mut assets: ResMut<CommanderAssets>,
) {
    *assets = CommanderAssets::default();
    let (Some(dir), Ok(info)) = (&level.dir, info.single()) else {
        return;
    };
    assets.desc = load_desc(dir);
    assets.instances = layout_assets(&level, info, &assets.desc);
    for asset in &mut assets.instances {
        asset.vehicle = paths.find(format!("vehicles/{}.ron", asset.template)).exists();
    }
    if assets.instances.is_empty() {
        return;
    }
    info!(
        "{} commander assets ({} vehicles)",
        assets.instances.len(),
        assets.instances.iter().filter(|a| a.vehicle).count()
    );
    // Objects are numbered by their place in the list: spawn each run of objects between
    // the vehicles with its first number.
    for run in assets.instances.chunk_by(|a, b| a.vehicle == b.vehicle) {
        if run[0].vehicle {
            continue;
        }
        let objects: Vec<StaticInstance> = run
            .iter()
            .map(|a| StaticInstance {
                template: a.template.clone(),
                placement: a.placement,
            })
            .collect();
        spawn_objects(&mut commands, &objects, run[0].instance, &paths);
    }
}
