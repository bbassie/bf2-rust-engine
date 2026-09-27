//! Interface images: the teams' flag icons and the vehicles' map icons (`.dds` paths relative
//! to the imported root), from BF2's HUD and menus.

use serde::{Deserialize, Serialize};

/// A team's flag icons. The neutral side only has `map`.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct TeamIcons {
    /// Small flag for the ticket bar, the flag pills and the scoreboard (BF2
    /// `scoreBoard_Flag`, 23x15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
    /// A control point held by the team on the map: a flag on a pole (`miniMap_CP`, 33x33).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map: Option<String>,
    /// The team's main base, which can't be captured (`miniMap_CPBase`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Square flag for menus (`joingame/flag_*`, 32x32).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu: Option<String>,
    /// Wide flag banner (`joingame/flagLarge_*`, 256x128).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub large: Option<String>,
}

/// What kind of vehicle it is, for maps (BF2 `vehicleHud.vehicleType`).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum VehicleClass {
    Tank,
    Apc,
    Helicopter,
    #[default]
    Jeep,
    Jet,
    AntiAir,
    Boat,
    /// Stationary weapons and the like (no `vehicleType`).
    Stationary,
}

/// How a vehicle shows on the maps.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct VehicleIcon {
    /// White silhouette, nose up, drawn in the team's colour (`vehicleHud.miniMapIcon`,
    /// 16x16). Without one the map draws a shape for the class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default)]
    pub class: VehicleClass,
    /// Its turret's direction is shown too (`vehicleHud.hasTurretIcon`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub turret: bool,
}
