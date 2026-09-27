//! The commander's assets: `levels/<name>/commander.ron`.
//!
//! Assets are objects the level's game mode layouts spawn (artillery guns, the UAV trailer,
//! the radar for satellite scans), found by their template among the layout's vehicle
//! spawners. While destroyed they can't be used. Supply drops need no asset.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::SoundDesc;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct CommanderDesc {
    /// Asset object templates (lowercase) and what they give the commander.
    #[serde(default)]
    pub assets: BTreeMap<String, AssetKind>,
    #[serde(default)]
    pub artillery: ArtilleryDesc,
    #[serde(default)]
    pub uav: UavDesc,
    #[serde(default)]
    pub supply: SupplyDesc,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssetKind {
    Artillery,
    Uav,
    /// The satellite scan's radar.
    Radar,
}

/// What each artillery gun fires at the target.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ArtilleryDesc {
    /// Shells per gun and seconds between them (BF2 `fire.burstSize`, `fire.roundsPerMinute`).
    pub shells: u32,
    pub interval: f32,
    /// They land at most this far from the target (`deviation.radius`).
    pub spread: f32,
    /// Each shell's blast (`detonation.explosionDamage`, `explosionRadius`,
    /// `explosionMaterial`) and effect (`endEffectTemplate`).
    pub damage: f32,
    pub radius: f32,
    pub material: u32,
    #[serde(default)]
    pub effect: Option<String>,
    /// The shells' whistle as they come down.
    #[serde(default)]
    pub incoming: Option<SoundDesc>,
}

impl Default for ArtilleryDesc {
    fn default() -> Self {
        Self {
            shells: 5,
            interval: 2.0,
            spread: 20.0,
            damage: 900.0,
            radius: 15.0,
            material: 44,
            effect: None,
            incoming: None,
        }
    }
}

/// The UAV circling over its target (BF2 `UAVControlObject`).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UavDesc {
    /// Meters around the target where it sees enemies (`uavVehicleRadius`).
    pub radius: f32,
    /// Its model (`uavVehicleTemplate`), `.glb`.
    #[serde(default)]
    pub mesh: Option<String>,
    /// The same as a vehicle (`vehicles/<name>.ron`): then it really flies its circle and
    /// can be shot down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle: Option<String>,
    /// Flight height and speed around the circle (`uavVehicleFlightHeight`, `uavVehicleSpeed`).
    pub height: f32,
    pub speed: f32,
}

impl Default for UavDesc {
    fn default() -> Self {
        Self {
            radius: 60.0,
            mesh: None,
            vehicle: None,
            height: 120.0,
            speed: 30.0,
        }
    }
}

/// The supply crate a drop brings (BF2 `SupplyObject supply_crate`).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SupplyDesc {
    /// `.glb` of the crate.
    #[serde(default)]
    pub mesh: Option<String>,
    /// Soldiers within `radius` meters get `heal` percent of their health and `ammo` percent
    /// of their ammunition per second (`radius`, `healSpeed`, `refillAmmoSpeed`), from a stock
    /// of `storage` percent points (`sharedStorageSize`); empty, the crate is gone.
    pub radius: f32,
    pub heal: f32,
    pub ammo: f32,
    pub storage: f32,
    /// It comes down from this height and lasts this long (`gameLogic.supplyDropHeight`,
    /// `supplyDropNumSecsToLive`).
    pub drop_height: f32,
    pub lifetime: f32,
}

impl Default for SupplyDesc {
    fn default() -> Self {
        Self {
            mesh: None,
            radius: 5.0,
            heal: 3.0,
            ammo: 7.0,
            storage: 500.0,
            drop_height: 50.0,
            lifetime: 300.0,
        }
    }
}
