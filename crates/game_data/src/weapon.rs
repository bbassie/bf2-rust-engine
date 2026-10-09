//! Kits (`kits/<name>.ron`) and handheld weapons (`weapons/<name>.ron`).

use serde::{Deserialize, Serialize};

use crate::SoundDesc;

/// A kit: the loadout of one soldier class.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct KitDesc {
    pub name: String,
    /// Class, e.g. `Assault`, `Medic`, `AT`.
    #[serde(default)]
    pub kind: String,
    /// Weapon names, in the order the kit lists them.
    pub weapons: Vec<String>,
    /// How fast the kit's ability charge (what thrown bags and shocks use up) refills, as
    /// a share per second (BF2 `abilityRestoreRate`). 0 without replenishing gadgets.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ability_restore: f32,
    /// Weapons the kit could be given instead of some of its own (BF2's unlocks: an
    /// `ItemContainer` with `unlockLevel`). Together with `kind` they say which weapons
    /// belong to the kit's class, for loadouts (see `game_shared::arsenal`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unlocks: Vec<KitUnlock>,
}

/// One of a kit's unlocks (BF2 `ItemContainer`): weapons it adds and the kit's own ones they
/// replace.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct KitUnlock {
    /// BF2 `unlockLevel`: 1 for the unlocks of BF2 1.5, 2 for those of Special Forces and
    /// the booster packs.
    pub level: u32,
    /// Weapons added (`addTemplate`).
    pub weapons: Vec<String>,
    /// Weapons of the kit (or of a lower unlock) they replace (`replaceItem`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FireMode {
    Single,
    Burst,
    Auto,
}

/// A handheld weapon.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WeaponDesc {
    pub name: String,
    /// Localization key or plain name.
    #[serde(default)]
    pub display_name: String,
    /// Inventory slot (BF2 `itemIndex`): 1 knife, 2 pistol, 3 primary, 4 grenade, 5+ gadgets.
    #[serde(default)]
    pub slot: u32,
    /// First-person and third-person models (`.glb`, one mesh per part), relative to the
    /// imported root. Third-person part `n` attaches to skeleton bone `mesh{n+1}`.
    #[serde(default)]
    pub mesh_1p: Option<String>,
    #[serde(default)]
    pub mesh_3p: Option<String>,
    /// Upper-body animation set (`.glb`) for third person.
    #[serde(default)]
    pub animations_3p: Option<String>,
    /// Arms-and-weapon animation set (`.glb`) for first person. First-person part `n`
    /// attaches to bone `mesh{n+1}` of the `1p_setup` skeleton.
    #[serde(default)]
    pub animations_1p: Option<String>,
    pub rounds_per_minute: f32,
    /// Selectable modes, first is the default.
    pub fire_modes: Vec<FireMode>,
    pub magazine_size: u32,
    pub magazines: u32,
    /// Seconds.
    pub reload_time: f32,
    /// Seconds until the weapon can be used after switching to it.
    pub deploy_time: f32,
    /// Projectiles per trigger pull (shotguns fire several).
    #[serde(default = "one")]
    pub projectiles_per_shot: u32,
    /// Spread of each pellet around the shot's direction, in degrees (BF2
    /// `deviation.subProjectileDev`).
    #[serde(default)]
    pub pellet_spread: f32,
    /// Bolt-action rifles: seconds from one shot until the next can fire (BF2
    /// `animation.shiftDelay`), when longer than `rounds_per_minute` allows.
    #[serde(default)]
    pub shift_delay: f32,
    /// Rounds loaded per `reload_time` (BF2 `ammo.reloadAmount`: shotguns load shell by
    /// shell). 0 fills the magazine at once.
    #[serde(default)]
    pub reload_amount: u32,
    /// How the weapon launches its projectiles: guns, throwing, placing charges.
    #[serde(default)]
    pub fire: FireDesc,
    /// Worn, not held (BF2 `isNightVision`, `isGasMask`): switched by keys of its own.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub worn: bool,
    /// Carried but never taken in hand: the parachute (BF2's `ParachuteLauncher`, a
    /// `SpawnObjectFireComp` without a `WeaponHud`), opened by the jump key.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// Icon for weapon lists: a white silhouette, about 128x44 (`.dds`, relative to the
    /// imported root; BF2 `weaponHud.selectIcon`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// C4: the detonator the hands hold instead while it is out (BF2 `fire.detonatorObject`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detonator: Option<DetonatorDesc>,
    pub projectile: ProjectileDesc,
    pub deviation: DeviationDesc,
    pub recoil: RecoilDesc,
    /// Field-of-view multipliers per zoom step; 0 means "not zoomed".
    #[serde(default)]
    pub zoom_factors: Vec<f32>,
    #[serde(default)]
    pub zoom: ZoomDesc,
    #[serde(default)]
    pub sounds: WeaponSounds,
    /// Medic bags, shock paddles, ammo bags and the wrench: what they heal, resupply,
    /// repair or revive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replenish: Option<ReplenishDesc>,
}

impl WeaponDesc {
    /// Whether the soldier can take it in hand: not worn gear (night vision, gas mask), not
    /// the parachute. Weapons imported before `hidden` existed are recognised by name.
    pub fn selectable(&self) -> bool {
        !self.worn && !self.hidden && !self.name.contains("parachute")
    }

    /// The knife (inventory slot 1): what the melee key takes out.
    pub fn is_melee(&self) -> bool {
        self.slot == 1 && self.fire.kind == FireKind::Gun && self.magazine_size == 0
    }

    /// Hand grenades (frag, smoke, flash bang, tear gas): thrown, bouncing, going off by
    /// themselves. Mines and charges are not.
    pub fn is_hand_grenade(&self) -> bool {
        self.fire.kind == FireKind::Thrown
            && self.projectile.impact == Impact::Bounce
            && self.projectile.trigger.is_none()
            && self.replenish.is_none()
    }

    /// A name fit to show (the kill feed, HUD): the resolved `display_name`, else the raw
    /// template name for the handful without one (see `bf2_import::weapons`).
    pub fn label(&self) -> &str {
        if self.display_name.is_empty() { &self.name } else { &self.display_name }
    }
}

/// Healing, resupplying, repairing and reviving (BF2 `ReplenishingAmmoComp` on the weapon,
/// `ReplenishDetonationComp` on thrown bags, `ResurrectCollisionComp` on the shock
/// paddles' projectile). Rates are percent of the target's maximum (hit points, or the
/// ammo it carries when full) per second, times the damage table factor of `material`
/// against the target's armor material, which is also what decides what can be healed
/// (soldiers) or repaired (vehicles, objects).
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct ReplenishDesc {
    pub kind: ReplenishKind,
    /// Damage table row (`ammo.abilityMaterial`).
    pub material: u32,
    /// In hand: what is within `radius` meters gets `strength` percent per second
    /// (`ammo.abilityRadius`, `ammo.abilityStrength`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub radius: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub strength: f32,
    /// Only while the trigger is held (`ammo.onlyActiveWhileFiring`: the wrench).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub while_firing: bool,
    /// Share of the kit's ability charge one use takes (a thrown bag, a shock:
    /// `ammo.abilityCost`), and one second of replenishing in hand (`ammo.abilityDrain`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cost: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub drain: f32,
    /// Thrown bags: a soldier within `pickup_radius` meters picks one up and gets
    /// `pickup_strength` percent at once (`detonation.triggerRadius`,
    /// `detonation.replenishingStrength`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pickup_radius: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pickup_strength: f32,
    /// Shock paddles: hit points a critically wounded teammate gets back
    /// (`collision.restoreHP`). 0 for everything else.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub revive_health: f32,
}

/// What a [`ReplenishDesc`] gives (BF2 `replenishingType`).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReplenishKind {
    /// Hit points: healing soldiers, repairing vehicles and objects (`RTHeal`).
    #[default]
    Health,
    /// Magazines, grenades and explosives (`RTAmmo`).
    Ammo,
}

/// How zooming looks (BF2 `DefaultZoomComp`).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ZoomDesc {
    /// First-person model that replaces `mesh_1p` while zoomed (BF2 `zoomLod`): the view
    /// through the scope, or the sights with a blurred rear sight. One part, on bone `mesh1`
    /// in the zoom pose. It is modelled for the unzoomed view model field of view.
    #[serde(default)]
    pub mesh_1p: Option<String>,
    /// Seconds from pressing zoom until `mesh_1p` replaces the weapon.
    #[serde(default)]
    pub delay: f32,
    /// Seconds from pressing zoom until the field of view narrows.
    #[serde(default)]
    pub fov_delay: f32,
    /// Bolt-action rifles leave the zoom after every shot.
    #[serde(default)]
    pub out_after_fire: bool,
    /// What the HUD shows while zoomed in instead of the crosshair (BF2's HUD for the
    /// weapon's `weaponHud.altGuiIndex`): the red dot of the M4, SCAR, P90 and AK-74U and
    /// their mods' kin, a launcher's sight. Empty for most, whose sights are in `mesh_1p`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sight: Vec<crate::HudPicture>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ProjectileDesc {
    /// Meters per second.
    pub velocity: f32,
    pub damage: f32,
    /// Damage never drops below this.
    pub min_damage: f32,
    /// Damage starts dropping at this distance (meters) and reaches `min_damage` at
    /// `falloff_end`. Both 0 = no falloff.
    #[serde(default)]
    pub falloff_start: f32,
    #[serde(default)]
    pub falloff_end: f32,
    /// Multiplier on gravity (bullets are ~1 in BF2 unless set).
    #[serde(default = "one_f")]
    pub gravity: f32,
    /// Seconds before the projectile disappears.
    pub time_to_live: f32,
    /// Material id in the damage table.
    #[serde(default)]
    pub material: u32,
    /// Rockets and grenades: damage at the center of the explosion, falling off to 0 at
    /// `explosion_radius` meters. 0 for bullets.
    #[serde(default)]
    pub explosion_damage: f32,
    #[serde(default)]
    pub explosion_radius: f32,
    /// Damage table row of the explosion (BF2 `detonation.explosionMaterial`), against the
    /// armor material of whatever is caught in it. `material` is for direct hits only.
    #[serde(default)]
    pub explosion_material: u32,
    /// Directional charges (claymores): the blast only reaches this many degrees off the
    /// charge's forward axis. 0 = all around.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub explosion_cone: f32,
    /// Effect where it detonates (`effects/<name>.ron`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detonation_effect: Option<String>,
    /// Effect that follows it in flight (rocket exhaust, grenade trails).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trail_effect: Option<String>,
    /// Model shown in flight and where it lies (`.glb`, relative to the imported root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh: Option<String>,
    /// What happens when it hits something.
    #[serde(default)]
    pub impact: Impact,
    /// Seconds after launch before it can detonate on impact or be triggered (BF2
    /// `detonation.timeUntilCanDetonate`, `armingDelay`). Earlier impacts bounce it off.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub arming_delay: f32,
    /// Rocket motor: `motor_delay` seconds after launch it accelerates by `acceleration`
    /// (m/s²) up to `max_speed` (m/s) (BF2 `startDelay`, `acceleration`, `maxSpeed`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub acceleration: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub max_speed: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub motor_delay: f32,
    /// Guided missiles: turn rate in radians per second (BF2 `follow.maxYaw`/`maxPitch`;
    /// the unit is inferred), and the distance to the aim point within which they stop
    /// steering (`follow.minDist`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub turn_rate: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub guidance_min_distance: f32,
    /// Mines and claymores: what sets them off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<TriggerDesc>,
    /// Smoke grenades: the cloud they leave when they go off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smoke: Option<SmokeDesc>,
    /// Where it sticks, a rope is strung (grappling hooks and ziplines).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rope: Option<RopeDesc>,
}

/// A rope soldiers climb or ride, strung by a projectile (BF2 SF's `GrapplingHookRope` and
/// `Zipline` templates).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RopeDesc {
    pub kind: RopeKind,
    /// Meters: grappling ropes hang at most this far down, ziplines reach at most this far.
    pub max_length: f32,
    /// Seconds until the rope is gone.
    pub lifetime: f32,
    /// Climbing speed on it, m/s (grappling ropes).
    #[serde(default)]
    pub climb_speed: f32,
    /// Grappling ropes: the links the rope is simulated as (`setNumberOfLinks`), each
    /// `max_length / links` long.
    #[serde(default = "default_rope_links")]
    pub links: u32,
    /// Share of the speed into a surface a link keeps bouncing off it (`elasticity`).
    #[serde(default = "default_rope_elasticity")]
    pub elasticity: f32,
    /// Share of a link's speed kept every 1/30 s in the air (`airFriction`).
    #[serde(default = "default_rope_air_friction")]
    pub air_friction: f32,
    /// Seconds the links move after being thrown or disturbed before they go to sleep
    /// (`AwakeTime`).
    #[serde(default = "default_rope_awake_time")]
    pub awake_time: f32,
    /// Radius of the rope (m), from its link model (`ropelink`).
    #[serde(default = "default_rope_radius")]
    pub radius: f32,
    /// The rope's color texture (`.dds`, relative to the imported root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub texture: Option<String>,
}

fn default_rope_links() -> u32 {
    26
}
fn default_rope_elasticity() -> f32 {
    0.2
}
fn default_rope_air_friction() -> f32 {
    0.95
}
fn default_rope_awake_time() -> f32 {
    6.0
}
fn default_rope_radius() -> f32 {
    0.02
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RopeKind {
    /// Hangs down from the hook: climbed up and down.
    Grapple,
    /// Stretched from the shooter to where it hit: slid down.
    Zipline,
}

impl ProjectileDesc {
    pub fn explodes(&self) -> bool {
        self.explosion_damage > 0.0 && self.explosion_radius > 0.0
    }

    /// Grenades, rockets and charges: objects that fly, bounce or lie around for a while and
    /// that everyone sees, unlike bullets (which clients show as tracers).
    pub fn is_object(&self) -> bool {
        self.explodes() || self.smoke.is_some() || self.trigger.is_some() || self.impact != Impact::Stop
    }

    /// Ends with a bang, a flash or smoke; bullets just stop.
    pub fn goes_off(&self) -> bool {
        self.explodes() || self.smoke.is_some() || (self.is_object() && self.detonation_effect.is_some())
    }
}

/// What a projectile does when it hits something.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq)]
pub enum Impact {
    /// Stops at the first thing it hits, hurting it and exploding if it can (bullets,
    /// rockets, rifle grenades).
    #[default]
    Stop,
    /// Bounces off everything until its fuse (`time_to_live`) runs out (hand grenades; BF2
    /// `collision.bouncing`).
    Bounce,
    /// Sticks to surfaces tilted at most `max_angle` degrees from level ground and bounces
    /// off steeper ones (C4, claymores; BF2 `StickyCollisionComp`).
    Stick { max_angle: f32 },
}

/// What sets a mine off (BF2 `detonation.trigger*`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TriggerDesc {
    pub by: TriggerBy,
    /// Meters.
    pub radius: f32,
    /// Only what is in front: at most this many degrees off the mine's forward axis.
    /// 0 = all around.
    #[serde(default)]
    pub angle: f32,
    /// Slower targets don't set it off (m/s).
    #[serde(default)]
    pub min_speed: f32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerBy {
    /// Soldiers of the other team (BF2 `MTYPco`).
    Soldiers,
    /// Anything heavy moving over it (BF2 `MTYVehicle`).
    Vehicles,
}

/// A cloud that hides what is behind it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SmokeDesc {
    /// Meters, once it has spread.
    pub radius: f32,
    /// Seconds from the grenade going off until the cloud is gone.
    pub duration: f32,
    /// Tear gas (BF2 `gasCloudType TearGas`): hit points per second it takes from those
    /// inside without a gas mask (`gasCloudDamage`). 0 = plain smoke.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub gas_damage: f32,
}

/// The model and first-person animations of a C4 detonator.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct DetonatorDesc {
    #[serde(default)]
    pub mesh_1p: Option<String>,
    #[serde(default)]
    pub animations_1p: Option<String>,
}

/// How a weapon launches its projectiles (BF2 fire and target components).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct FireDesc {
    #[serde(default)]
    pub kind: FireKind,
    /// Thrown weapons: seconds of winding up before the throw can leave (BF2
    /// `fire.pullBackTime`). Holding on longer cooks a grenade: its fuse is running.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pull_back: f32,
    /// Seconds from letting go of the trigger (thrown weapons) or pulling it (charges)
    /// until the projectile leaves the hand (`fire.fireLaunchDelay`), and for the underhand
    /// throw on the alternative fire button (`fire.fireLaunchDelaySoft`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub launch_delay: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub launch_delay_soft: f32,
    /// Where projectiles start, relative to the eye in view space (+X right, +Y up,
    /// -Z forward) (`fire.projectileStartPosition`).
    #[serde(default)]
    pub start_offset: [f32; 3],
    /// Most projectiles of one soldier in the world at once (`fire.maxProjectilesInWorld`):
    /// the oldest goes when another is placed. 0 = no limit.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub max_in_world: u32,
    #[serde(default)]
    pub guidance: Guidance,
    /// Heat seekers: locking on before firing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<LockDesc>,
    /// Vehicle machine guns: heat per shot (1 overheats), cooling per second, and seconds it
    /// can't fire once overheated (BF2 `heatAddWhenFire`, `coolDownPerSec`, `overheatPenalty`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overheat: Option<OverheatDesc>,
}

/// Locking a heat seeker on: the target must stay within `angle` degrees of the sight and
/// `range` meters for `time` seconds (BF2 `target.lockDelay`, `lockAngle`, `maxDistance`).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct LockDesc {
    pub time: f32,
    pub angle: f32,
    pub range: f32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct OverheatDesc {
    pub per_shot: f32,
    pub cooling: f32,
    pub penalty: f32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FireKind {
    /// Fires while the trigger is pulled, by fire mode.
    #[default]
    Gun,
    /// Grenades and mines: hold the trigger to wind up (and cook a grenade), let go to throw
    /// (BF2 `ThrownFireComp`). The alternative fire button throws underhand.
    Thrown,
    /// C4: the trigger throws charges; the alternative fire button takes out the detonator,
    /// whose trigger sets them all off (BF2 `ExplosivesFireComp`).
    Explosives,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Guidance {
    #[default]
    None,
    /// Wire guided (BF2 `TSWireGuided`, and TV and laser guided): flies towards whatever the
    /// shooter aims at while he keeps the launcher in his hands or stays at the sight.
    Wire,
    /// Heat seeking (BF2 `TSHeatSeeking`): flies towards the aircraft it was locked on to.
    Heat,
}

fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

/// BF2 deviation (spread) settings, in degrees. See docs/formats/gameplay-data.md §6.4.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DeviationDesc {
    /// Base cone.
    pub min: f32,
    pub stand: f32,
    pub crouch: f32,
    pub prone: f32,
    pub zoom: f32,
    /// Added per shot: `[max, add, decay per 1/30 s]`.
    pub fire: [f32; 3],
    /// From movement: `[max, per forward speed, per strafe speed, decay]`.
    pub speed: [f32; 4],
    /// From jumping: `[max, add, decay]`.
    pub misc: [f32; 3],
}

impl Default for DeviationDesc {
    fn default() -> Self {
        Self {
            min: 0.5,
            stand: 1.0,
            crouch: 1.0,
            prone: 1.0,
            zoom: 1.0,
            fire: [0.0; 3],
            speed: [0.0; 4],
            misc: [0.0; 3],
        }
    }
}

/// Camera kick per shot in degrees, as uniform ranges.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct RecoilDesc {
    pub up: [f32; 2],
    pub left_right: [f32; 2],
    #[serde(default = "one_f")]
    pub zoom_modifier: f32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct WeaponSounds {
    /// Every shot, heard by the shooter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fire_1p: Option<SoundDesc>,
    /// Every shot, heard by everyone else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fire_3p: Option<SoundDesc>,
    /// `fire_3p` as heard from far away: muffled, with echoes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fire_3p_distant: Option<SoundDesc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reload_1p: Option<SoundDesc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reload_3p: Option<SoundDesc>,
    /// Taking the weapon out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy_1p: Option<SoundDesc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy_3p: Option<SoundDesc>,
    /// Pulling the trigger with nothing left to fire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_fire: Option<SoundDesc>,
    /// The bolt catching after the last round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bolt: Option<SoundDesc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_fire_mode: Option<SoundDesc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom: Option<SoundDesc>,
}

fn one() -> u32 {
    1
}

fn one_f() -> f32 {
    1.0
}
