//! Game modes: their ids and rules, and the layouts of the attack/defend modes.
//!
//! | id | mode | rules |
//! |---|---|---|
//! | `gpm_cq` | Conquest | BF2's: the team with more soldiers at a flag takes it, area value bleeds tickets |
//! | `gpm_coop` | Co-op | conquest, with the humans on one team and bots filling both |
//! | `gpm_rush` | Rush | the attackers arm pairs of charges, stage by stage; the defenders defuse them |
//! | `gpm_breakthrough` | Breakthrough | the attackers take sectors of flags; the defenders fall back |
//! | `gpm_tdm` | Team Deathmatch | every death costs the team a ticket; flags only mark spawns (AIX 2's layouts) |
//!
//! In both staged modes only the attackers have tickets. Taking a stage moves the front (both
//! sides spawn further on) and refills them; the attackers win by taking the last stage, the
//! defenders when the attackers run out.
//!
//! BF2's levels have conquest and co-op layouts only. [`complete_layouts`] adds Rush and
//! Breakthrough layouts made from the conquest ones ([`generate`]) wherever a level has none
//! of its own: the control points are put in order along the way from the attackers' base to
//! the defenders', grouped into stages, and a pair of charges placed beside each stage's flags.
//! Layouts written by hand (in `level.ron` or `levels/<name>/modes.ron`, see [`ModeLayouts`])
//! take their place.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::{ControlPointDesc, GameModeDesc};

/// BF2's conquest.
pub const CONQUEST: &str = "gpm_cq";
/// BF2's co-op.
pub const COOP: &str = "gpm_coop";
pub const RUSH: &str = "gpm_rush";
pub const BREAKTHROUGH: &str = "gpm_breakthrough";
/// Team deathmatch, as mods like AIX 2 have it.
pub const TEAM_DEATHMATCH: &str = "gpm_tdm";

/// The rules a mode plays by.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ModeKind {
    #[default]
    Conquest,
    Coop,
    Rush,
    Breakthrough,
    TeamDeathmatch,
}

impl ModeKind {
    pub const ALL: [ModeKind; 5] =
        [ModeKind::Conquest, ModeKind::Coop, ModeKind::Rush, ModeKind::Breakthrough, ModeKind::TeamDeathmatch];

    /// The mode of an id (as typed, see [`canonical_mode`]); `None` for modes the game doesn't
    /// know.
    pub fn known(mode: &str) -> Option<Self> {
        let id = canonical_mode(mode);
        Self::ALL.into_iter().find(|kind| kind.id() == id)
    }

    /// The rules of a mode id. Modes the game doesn't know (BF2's `gpm_ctf`, the singleplayer
    /// layouts) play by conquest's.
    pub fn of(mode: &str) -> Self {
        Self::known(mode).unwrap_or_default()
    }

    pub fn id(self) -> &'static str {
        match self {
            ModeKind::Conquest => CONQUEST,
            ModeKind::Coop => COOP,
            ModeKind::Rush => RUSH,
            ModeKind::Breakthrough => BREAKTHROUGH,
            ModeKind::TeamDeathmatch => TEAM_DEATHMATCH,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ModeKind::Conquest => "Conquest",
            ModeKind::Coop => "Co-op",
            ModeKind::Rush => "Rush",
            ModeKind::Breakthrough => "Breakthrough",
            ModeKind::TeamDeathmatch => "Team Deathmatch",
        }
    }

    /// One line for menus.
    pub fn description(self) -> &'static str {
        match self {
            ModeKind::Conquest => "Hold more flags than the enemy to bleed his tickets.",
            ModeKind::Coop => "Conquest with the humans on one team; bots fill both teams.",
            ModeKind::Rush => {
                "Attackers arm two charges per stage, defenders defuse them. Destroy the last pair to win."
            }
            ModeKind::Breakthrough => {
                "Attackers take every flag of a sector to push the front; defenders fall back to the next."
            }
            ModeKind::TeamDeathmatch => "Every death costs the team a ticket. No flags to take.",
        }
    }

    /// Attack/defend in stages, with a [`StagedDesc`] layout.
    pub fn staged(self) -> bool {
        matches!(self, ModeKind::Rush | ModeKind::Breakthrough)
    }

    /// What a stage is called on the HUD.
    pub fn stage_noun(self) -> &'static str {
        match self {
            ModeKind::Breakthrough => "Sector",
            _ => "Stage",
        }
    }
}

/// A mode's id as someone may type it: `rush`, `Rush` and `gpm_rush` are all `gpm_rush`, `cq`
/// and `conquest` are `gpm_cq`, `bt` is `gpm_breakthrough`. Other names stay as they are.
pub fn canonical_mode(name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase();
    let short = lower.strip_prefix("gpm_").unwrap_or(&lower);
    match short {
        "cq" | "conquest" => CONQUEST.into(),
        "coop" | "co-op" => COOP.into(),
        "rush" => RUSH.into(),
        "breakthrough" | "bt" => BREAKTHROUGH.into(),
        "tdm" | "deathmatch" | "teamdeathmatch" => TEAM_DEATHMATCH.into(),
        _ => lower,
    }
}

/// Human readable name of a mode id: `gpm_cq` -> `Conquest`.
pub fn mode_label(mode: &str) -> String {
    match ModeKind::known(mode) {
        Some(kind) => kind.label().into(),
        None => match mode {
            "gpm_ctf" => "Capture the Flag".into(),
            "sp1" | "sp2" | "sp3" => "Singleplayer".into(),
            other => other.trim_start_matches("gpm_").to_uppercase(),
        },
    }
}

/// A layout of a level by mode and size.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LayoutRef {
    pub mode: String,
    pub size: u32,
}

/// An attack/defend layout (Rush, Breakthrough): [`GameModeDesc::staged`].
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StagedDesc {
    /// The attacking team, 1 or 2.
    pub attacker: u8,
    /// The attackers' tickets at the start, and again after every stage they take.
    pub tickets: f32,
    /// In the order they are fought over; the attackers win by taking the last.
    pub stages: Vec<StageDesc>,
    /// Rush: seconds of holding the use key at a charge to arm it and to defuse it, and the
    /// seconds an armed charge takes to go off.
    #[serde(default = "default_arm_seconds")]
    pub arm_seconds: f32,
    #[serde(default = "default_defuse_seconds")]
    pub defuse_seconds: f32,
    #[serde(default = "default_fuse_seconds")]
    pub fuse_seconds: f32,
}

/// One stage of a staged layout.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct StageDesc {
    /// Shown on the HUD; `Stage 2` / `Sector 2` if empty.
    #[serde(default)]
    pub name: String,
    /// Rush: the charges to destroy (all of them).
    #[serde(default)]
    pub charges: Vec<ChargeDesc>,
    /// Breakthrough: the control points (ids) to hold at once. Points of later sectors can't
    /// be captured before; taken ones stay the attackers'.
    #[serde(default)]
    pub control_points: Vec<String>,
    /// Rush: the control points (ids) each side spawns at while this stage is played (points
    /// in neither list are neutral). Breakthrough goes by who holds the flags instead.
    #[serde(default)]
    pub attacker_spawns: Vec<String>,
    #[serde(default)]
    pub defender_spawns: Vec<String>,
}

/// A charge (an "M-COM station"): the attackers arm it, the defenders defuse it, and once the
/// fuse runs out it is destroyed.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChargeDesc {
    /// `A`, `B`.
    pub name: String,
    /// Where it stands (its foot).
    pub position: [f32; 3],
    /// Which way its front faces: degrees, counter-clockwise from north (-Z) seen from above.
    #[serde(default)]
    pub yaw: f32,
    /// The control point it stands at, if any.
    #[serde(default)]
    pub control_point: Option<String>,
    /// Object template drawn for it (`templates/<name>.ron`); a marker of the game's own
    /// without one.
    #[serde(default)]
    pub template: Option<String>,
    /// Placed by [`generate`] near its control point: the server moves it onto ground soldiers
    /// can reach. Hand-placed charges stay where they are.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub approximate: bool,
}

fn default_arm_seconds() -> f32 {
    4.0
}

fn default_defuse_seconds() -> f32 {
    6.0
}

fn default_fuse_seconds() -> f32 {
    30.0
}

/// `levels/<name>/modes.ron`: layouts that replace the level's (same mode and size) or add to
/// them. A mod can ship one for an imported level without touching its `level.ron`, and a
/// layout [`based on`](Self) another takes the control points, spawn points and vehicle
/// spawners it doesn't list from that one, so it holds nothing but its own additions.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ModeLayouts {
    #[serde(default)]
    pub game_modes: Vec<GameModeDesc>,
}

/// Puts `extra` into `layouts`: each replaces a layout of the same mode and size or is added.
/// Layouts `based_on` another get what they leave out from it.
pub fn merge_layouts(layouts: &mut Vec<GameModeDesc>, extra: Vec<GameModeDesc>) {
    for mut layout in extra {
        layout.mode = canonical_mode(&layout.mode);
        if let Some(base) = layout.based_on.as_ref().and_then(|b| find(layouts, &b.mode, b.size)).cloned() {
            if layout.control_points.is_empty() {
                layout.control_points = base.control_points;
            }
            if layout.spawn_points.is_empty() {
                layout.spawn_points = base.spawn_points;
            }
            if layout.vehicle_spawners.is_empty() {
                layout.vehicle_spawners = base.vehicle_spawners;
            }
            if layout.statics.is_empty() {
                layout.statics = base.statics;
            }
            if layout.combat_areas.is_empty() {
                layout.combat_areas = base.combat_areas;
            }
        }
        match layouts.iter_mut().find(|l| l.mode == layout.mode && l.size == layout.size) {
            Some(existing) => *existing = layout,
            None => layouts.push(layout),
        }
    }
}

/// The layout of `mode` with exactly `size`.
fn find<'a>(layouts: &'a [GameModeDesc], mode: &str, size: u32) -> Option<&'a GameModeDesc> {
    layouts.iter().find(|l| l.mode == mode && l.size == size)
}

/// Adds a generated Rush and Breakthrough layout for every conquest layout (co-op, for levels
/// without conquest) whose size has none of that mode yet.
pub fn complete_layouts(layouts: &mut Vec<GameModeDesc>) {
    let mut sources: Vec<GameModeDesc> = layouts.iter().filter(|l| l.mode == CONQUEST).cloned().collect();
    if sources.is_empty() {
        sources = layouts.iter().filter(|l| l.mode == COOP).cloned().collect();
    }
    for kind in [ModeKind::Rush, ModeKind::Breakthrough] {
        for source in &sources {
            if find(layouts, kind.id(), source.size).is_none()
                && let Some(layout) = generate(source, kind)
            {
                layouts.push(layout);
            }
        }
    }
}

/// Attacker tickets per stage for a generated layout: about three for every attacker in the
/// Rush, a few more in Breakthrough, where flags take longer than charges.
pub fn default_tickets(kind: ModeKind, size: u32) -> f32 {
    let tickets = match kind {
        ModeKind::Breakthrough => 75.0 + 3.0 * size as f32,
        _ => 50.0 + 2.5 * size as f32,
    };
    (tickets / 5.0).round() * 5.0
}

/// A Rush or Breakthrough layout made from `source` (a conquest layout), or `None` when it
/// has nothing to fight over between the bases.
///
/// - **Sides**: the attackers are the side holding fewer flags at the start (at Karkand the US
///   at the gas station, attacking the MEC's town); on a tie the side starting nearer the
///   flags between them (Dalian: the airfield, not the carrier), else team 2. Their bases and
///   flags are where they start from.
/// - **Order**: the other flags, by how far along the way from the attackers' start to the
///   defenders' main base (or the flag farthest away) they are, measured over the links
///   between neighbouring flags (the relative neighbourhood graph: two flags are neighbours
///   unless a third is closer to both).
/// - **Stages**: groups of neighbouring flags in that order, one to three each and at most five
///   stages (short layouts get one flag per stage).
/// - **Charges** (Rush): at a stage of one flag, one on each side of it across the line of
///   attack; at a stage of several, beside two of them about 80 m apart. The server moves them
///   onto ground soldiers can walk to ([`ChargeDesc::approximate`]).
/// - **Spawns** (Rush): the attackers hold their start and every stage taken so far, the
///   defenders the current stage's flags, the stages behind it and their bases (their
///   vehicles spawn there). The server lets each side spawn only at those near the stage's
///   charges, and closes a flag for spawning while enemies are at it (`game_server`'s
///   `modes::staged`).
pub fn generate(source: &GameModeDesc, kind: ModeKind) -> Option<GameModeDesc> {
    if !kind.staged() {
        return None;
    }
    let front = Front::new(source, kind)?;
    let points = &source.control_points;
    let ids = |group: &[usize]| -> Vec<String> { group.iter().map(|&i| points[i].id.clone()).collect() };
    let defender = 3 - front.attacker;
    let mut control_points: Vec<ControlPointDesc> = points.clone();
    let mut stages = Vec::new();
    for (s, group) in front.stages.iter().enumerate() {
        let mut stage = StageDesc {
            name: format!("{} {}", kind.stage_noun(), s + 1),
            ..Default::default()
        };
        match kind {
            ModeKind::Rush => {
                stage.charges = place_charges(points, group, front.directions[s]);
                let mut attackers = front.start.clone();
                attackers.extend(front.stages[..s].iter().flatten());
                let mut defenders: Vec<usize> = front.stages[s..].iter().flatten().copied().collect();
                defenders.extend(&front.bases);
                stage.attacker_spawns = ids(&attackers);
                stage.defender_spawns = ids(&defenders);
            }
            _ => stage.control_points = ids(group),
        }
        stages.push(stage);
    }
    // The first stage's sides, so the layout's points show who holds what when it starts.
    for (i, cp) in control_points.iter_mut().enumerate() {
        match kind {
            ModeKind::Rush => {
                // Nothing is captured in the Rush: flags only say where each side spawns.
                cp.uncapturable = true;
                cp.initial_team = if stages[0].attacker_spawns.contains(&cp.id) {
                    front.attacker
                } else if stages[0].defender_spawns.contains(&cp.id) {
                    defender
                } else {
                    0
                };
            }
            _ => {
                if front.start.contains(&i) {
                    cp.initial_team = front.attacker;
                } else if front.stages.iter().flatten().any(|&p| p == i) || front.bases.contains(&i) {
                    cp.initial_team = defender;
                }
            }
        }
    }
    Some(GameModeDesc {
        mode: kind.id().into(),
        size: source.size,
        control_points,
        spawn_points: source.spawn_points.clone(),
        vehicle_spawners: source.vehicle_spawners.clone(),
        statics: source.statics.clone(),
        combat_areas: source.combat_areas.clone(),
        staged: Some(StagedDesc {
            attacker: front.attacker,
            tickets: default_tickets(kind, source.size),
            stages,
            arm_seconds: default_arm_seconds(),
            defuse_seconds: default_defuse_seconds(),
            fuse_seconds: default_fuse_seconds(),
        }),
        based_on: Some(LayoutRef {
            mode: source.mode.clone(),
            size: source.size,
        }),
        generated: true,
    })
}

/// The two sides of a layout and the flags between them, in stages.
struct Front {
    attacker: u8,
    /// Control points (indices) the attackers start with: their bases and flags.
    start: Vec<usize>,
    /// The defenders' main bases.
    bases: Vec<usize>,
    /// Per stage, its control points.
    stages: Vec<Vec<usize>>,
    /// Per stage, the direction of the attack on it (XZ, unit length).
    directions: Vec<[f32; 2]>,
}

impl Front {
    fn new(layout: &GameModeDesc, kind: ModeKind) -> Option<Self> {
        let points = &layout.control_points;
        let has_spawns = |cp: &ControlPointDesc| cp.uncapturable || layout.spawn_points.iter().any(|s| s.control_point == cp.id);
        let held = |team: u8| points.iter().filter(|cp| !cp.uncapturable && cp.initial_team == team).count();
        let can_start = |team: u8| points.iter().any(|cp| cp.initial_team == team && has_spawns(cp));
        // On a tie, the side starting nearer the flags between them (a land base rather than a
        // carrier far out at sea: bots cut off from the front on a carrier soon run out of
        // boats and aircraft); else team 2.
        let reach = |team: u8| {
            let starts = points.iter().filter(|cp| cp.initial_team == team);
            starts
                .flat_map(|s| {
                    points
                        .iter()
                        .filter(|cp| !cp.uncapturable && cp.initial_team != team)
                        .map(move |cp| distance(xz(s.position), xz(cp.position)))
                })
                .fold(f32::INFINITY, f32::min)
        };
        let mut attacker = match held(1).cmp(&held(2)) {
            Ordering::Less => 1,
            Ordering::Greater => 2,
            Ordering::Equal if reach(1) + 50.0 < reach(2) => 1,
            Ordering::Equal => 2,
        };
        if !can_start(attacker) {
            attacker = 3 - attacker;
        }
        if !can_start(attacker) {
            return None;
        }
        let defender = 3 - attacker;
        let indices = |f: &dyn Fn(&ControlPointDesc) -> bool| -> Vec<usize> {
            points.iter().enumerate().filter(|(_, cp)| f(cp)).map(|(i, _)| i).collect()
        };
        let start = indices(&|cp| cp.initial_team == attacker);
        let bases = indices(&|cp| cp.uncapturable && cp.initial_team == defender);
        // Not points sharing their id with others (AIX 2's Trident has hundreds of "Aircraft"
        // points in the air, all "1"): nobody could tell them apart, spawn points included.
        let unique = |cp: &ControlPointDesc| points.iter().filter(|other| other.id == cp.id).count() == 1;
        let mut front = indices(&|cp| !cp.uncapturable && cp.initial_team != attacker && unique(cp));
        if front.is_empty() {
            return None;
        }

        let positions: Vec<[f32; 2]> = points.iter().map(|cp| xz(cp.position)).collect();
        let graph = neighbourhood(&positions);
        let from_start = shortest(&graph, &start);
        let end = if bases.is_empty() {
            vec![*front.iter().max_by(|&&a, &&b| from_start[a].total_cmp(&from_start[b]))?]
        } else {
            bases.clone()
        };
        let from_end = shortest(&graph, &end);
        let progress = |i: usize| from_start[i] / (from_start[i] + from_end[i]).max(1e-3);
        front.sort_by(|&a, &b| progress(a).total_cmp(&progress(b)).then(a.cmp(&b)));

        let mut stages = Vec::new();
        let mut rest = front.as_slice();
        for size in stage_sizes(front.len(), kind) {
            let (group, tail) = rest.split_at(size);
            stages.push(group.to_vec());
            rest = tail;
        }
        let overall = direction(centroid(points, &start), centroid(points, &end)).unwrap_or([0.0, -1.0]);
        let directions = (0..stages.len())
            .map(|s| {
                let to = centroid(points, &stages[s]);
                let from = match s {
                    // From the nearest point they start at: a carrier's fleet is far behind.
                    0 => start
                        .iter()
                        .map(|&i| xz(points[i].position))
                        .min_by(|a, b| distance(*a, to).total_cmp(&distance(*b, to)))
                        .unwrap_or(to),
                    _ => centroid(points, &stages[s - 1]),
                };
                direction(from, to).unwrap_or(overall)
            })
            .collect();
        Some(Self {
            attacker,
            start,
            bases,
            stages,
            directions,
        })
    }
}

/// How many control points each stage gets: as even as possible, bigger stages first.
fn stage_sizes(n: usize, kind: ModeKind) -> Vec<usize> {
    let singles = if kind == ModeKind::Rush { 4 } else { 3 };
    let count = if n <= singles { n } else { n.div_ceil(2).clamp(2, 5) };
    (0..count).map(|i| n / count + usize::from(i < n % count)).collect()
}

/// A stage's charges: see [`generate`].
fn place_charges(points: &[ControlPointDesc], group: &[usize], forward: [f32; 2]) -> Vec<ChargeDesc> {
    // Seen from the attackers, facing `forward`.
    let left = [forward[1], -forward[0]];
    // Facing the attackers.
    let yaw = forward[0].atan2(forward[1]).to_degrees();
    let at = |cp: &ControlPointDesc, side: f32, back: f32| -> [f32; 3] {
        [
            cp.position[0] + left[0] * side + forward[0] * back,
            cp.position[1],
            cp.position[2] + left[1] * side + forward[1] * back,
        ]
    };
    let charge = |name: &str, cp: &ControlPointDesc, position: [f32; 3]| ChargeDesc {
        name: name.into(),
        position,
        yaw,
        control_point: Some(cp.id.clone()),
        template: None,
        approximate: true,
    };
    match group {
        [] => Vec::new(),
        [single] => {
            let cp = &points[*single];
            let side = (cp.radius * 1.6).clamp(10.0, 22.0);
            vec![charge("A", cp, at(cp, side, 3.0)), charge("B", cp, at(cp, -side, 3.0))]
        }
        _ => {
            // The pair closest to 80 m apart.
            let mut best = (group[0], group[1], f32::MAX);
            for (n, &a) in group.iter().enumerate() {
                for &b in &group[n + 1..] {
                    let off = (distance(xz(points[a].position), xz(points[b].position)) - 80.0).abs();
                    if off < best.2 {
                        best = (a, b, off);
                    }
                }
            }
            let (mut a, mut b) = (&points[best.0], &points[best.1]);
            let side = |cp: &ControlPointDesc| cp.position[0] * left[0] + cp.position[2] * left[1];
            if side(b) > side(a) {
                std::mem::swap(&mut a, &mut b);
            }
            vec![charge("A", a, at(a, 0.0, 4.0)), charge("B", b, at(b, 0.0, 4.0))]
        }
    }
}

fn xz(p: [f32; 3]) -> [f32; 2] {
    [p[0], p[2]]
}

fn distance(a: [f32; 2], b: [f32; 2]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

fn centroid(points: &[ControlPointDesc], group: &[usize]) -> [f32; 2] {
    let n = group.len().max(1) as f32;
    let sum = group.iter().fold([0.0, 0.0], |s, &i| [s[0] + points[i].position[0], s[1] + points[i].position[2]]);
    [sum[0] / n, sum[1] / n]
}

/// Unit vector from `a` to `b`, if they are apart.
fn direction(a: [f32; 2], b: [f32; 2]) -> Option<[f32; 2]> {
    let d = distance(a, b);
    (d > 1.0).then(|| [(b[0] - a[0]) / d, (b[1] - a[1]) / d])
}

/// The relative neighbourhood graph of `points`: two points are neighbours unless a third is
/// closer to both. Per point, its neighbours and how far they are.
fn neighbourhood(points: &[[f32; 2]]) -> Vec<Vec<(usize, f32)>> {
    let n = points.len();
    let mut graph = vec![Vec::new(); n];
    for a in 0..n {
        for b in a + 1..n {
            let ab = distance(points[a], points[b]);
            let blocked = (0..n).any(|c| c != a && c != b && distance(points[a], points[c]).max(distance(points[b], points[c])) < ab);
            if !blocked {
                graph[a].push((b, ab));
                graph[b].push((a, ab));
            }
        }
    }
    graph
}

/// Shortest distances over `graph` from the nearest of `sources`.
fn shortest(graph: &[Vec<(usize, f32)>], sources: &[usize]) -> Vec<f32> {
    let mut dist = vec![f32::INFINITY; graph.len()];
    let mut done = vec![false; graph.len()];
    for &s in sources {
        dist[s] = 0.0;
    }
    // Few points: a plain O(n²) Dijkstra.
    while let Some(u) = (0..graph.len()).filter(|&i| !done[i] && dist[i].is_finite()).min_by(|&a, &b| dist[a].total_cmp(&dist[b])) {
        done[u] = true;
        for &(v, w) in &graph[u] {
            if dist[u] + w < dist[v] {
                dist[v] = dist[u] + w;
            }
        }
    }
    dist
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Placement, SpawnPointDesc};

    fn cp(id: &str, x: f32, z: f32, team: u8, base: bool) -> ControlPointDesc {
        ControlPointDesc {
            id: id.into(),
            name: id.to_uppercase(),
            position: [x, 100.0, z],
            initial_team: team,
            radius: 10.0,
            uncapturable: base,
            area_value: [0.0; 2],
            time_to_get_control: 10.0,
            time_to_lose_control: 10.0,
            only_takeable_by_team: 0,
            enemy_ticket_loss_when_captured: 0.0,
        }
    }

    fn layout(points: Vec<ControlPointDesc>) -> GameModeDesc {
        let spawn_points = points
            .iter()
            .map(|p| SpawnPointDesc {
                control_point: p.id.clone(),
                placement: Placement {
                    position: p.position,
                    ..Default::default()
                },
            })
            .collect();
        GameModeDesc {
            mode: CONQUEST.into(),
            size: 32,
            control_points: points,
            spawn_points,
            ..Default::default()
        }
    }

    /// Karkand-like: a team 2 base in the south, a chain of team 1 flags going north with
    /// one off to the side, no team 1 base.
    fn karkand() -> GameModeDesc {
        layout(vec![
            cp("gas", 0.0, 500.0, 2, true),
            cp("hotel", 0.0, 250.0, 1, false),
            cp("square", -30.0, 160.0, 1, false),
            cp("market", 0.0, 70.0, 1, false),
            cp("suburb", -150.0, 20.0, 1, false),
            cp("factory", 200.0, -120.0, 1, false),
        ])
    }

    #[test]
    fn mode_names() {
        assert_eq!(canonical_mode("Rush"), RUSH);
        assert_eq!(canonical_mode("bt"), BREAKTHROUGH);
        assert_eq!(canonical_mode("gpm_cq"), CONQUEST);
        assert_eq!(ModeKind::of("gpm_ctf"), ModeKind::Conquest);
        assert_eq!(mode_label("gpm_breakthrough"), "Breakthrough");
        assert_eq!(mode_label("gpm_ctf"), "Capture the Flag");
    }

    #[test]
    fn orders_the_flags_from_base_to_base() {
        let front = Front::new(&karkand(), ModeKind::Breakthrough).unwrap();
        assert_eq!(front.attacker, 2);
        assert_eq!(front.start, [0]);
        assert!(front.bases.is_empty());
        // Five flags: three sectors of 2, 2, 1, from the gas station northwards.
        let source = karkand();
        let names: Vec<Vec<&str>> = front
            .stages
            .iter()
            .map(|g| {
                let mut names: Vec<&str> = g.iter().map(|&i| source.control_points[i].id.as_str()).collect();
                names.sort();
                names
            })
            .collect();
        assert_eq!(names, [vec!["hotel", "square"], vec!["market", "suburb"], vec!["factory"]]);
        // The first attack goes north (-Z).
        assert!(front.directions[0][1] < -0.9, "{:?}", front.directions[0]);
    }

    #[test]
    fn rush_layout() {
        let source = karkand();
        let rush = generate(&source, ModeKind::Rush).unwrap();
        let staged = rush.staged.as_ref().unwrap();
        assert_eq!(staged.attacker, 2);
        assert_eq!(staged.stages.len(), 3);
        assert_eq!(rush.based_on, Some(LayoutRef { mode: CONQUEST.into(), size: 32 }));
        for stage in &staged.stages {
            assert_eq!(stage.charges.len(), 2);
        }
        // Stage 1: hotel and square, a charge beside each, A on the attackers' left (west).
        let first = &staged.stages[0];
        assert_eq!(first.charges[0].control_point.as_deref(), Some("square"));
        assert_eq!(first.charges[1].control_point.as_deref(), Some("hotel"));
        // Attackers at the gas station, defenders at every later flag.
        assert_eq!(first.attacker_spawns, ["gas"]);
        let mut defenders = first.defender_spawns.clone();
        defenders.sort();
        assert_eq!(defenders, ["factory", "hotel", "market", "square", "suburb"]);
        // The last stage (a single flag, both charges beside it): defenders spawn at it.
        let last = staged.stages.last().unwrap();
        assert_eq!(last.defender_spawns, ["factory"]);
        assert!(last.attacker_spawns.contains(&"hotel".to_string()));
        let [a, b] = &last.charges[..] else { unreachable!() };
        let apart = distance(xz(a.position), xz(b.position));
        assert!((20.0..=44.0).contains(&apart), "{apart}");
        // Flags only say where each side spawns now.
        let owner = |id: &str| rush.control_points.iter().find(|c| c.id == id).unwrap().initial_team;
        assert_eq!((owner("gas"), owner("hotel"), owner("market")), (2, 1, 1));
        assert!(rush.control_points.iter().all(|c| c.uncapturable));
    }

    #[test]
    fn breakthrough_layout() {
        let bt = generate(&karkand(), ModeKind::Breakthrough).unwrap();
        let staged = bt.staged.unwrap();
        assert_eq!(staged.stages[0].control_points, ["hotel", "square"]);
        assert_eq!(staged.stages[0].name, "Sector 1");
        // Everything but the attackers' base starts with the defenders.
        assert!(bt.control_points.iter().all(|c| c.initial_team == if c.id == "gas" { 2 } else { 1 }));
    }

    #[test]
    fn the_side_holding_less_attacks() {
        // Both sides have a base; team 1 holds two flags, team 2 one: team 2 attacks, and its
        // flag is a place it starts from.
        let source = layout(vec![
            cp("one", 0.0, 0.0, 1, true),
            cp("a", 0.0, 100.0, 1, false),
            cp("b", 0.0, 200.0, 1, false),
            cp("c", 0.0, 300.0, 0, false),
            cp("d", 0.0, 400.0, 2, false),
            cp("two", 0.0, 500.0, 2, true),
        ]);
        let front = Front::new(&source, ModeKind::Rush).unwrap();
        assert_eq!(front.attacker, 2);
        assert_eq!(front.start, [4, 5]);
        assert_eq!(front.bases, [0]);
        assert_eq!(front.stages, [vec![3], vec![2], vec![1]]);
    }

    #[test]
    fn a_tie_goes_to_the_side_nearer_the_front() {
        // Dalian-like: a land base in the west, a carrier far out in the east, neutral flags
        // between: the land side attacks.
        let source = layout(vec![
            cp("airfield", -700.0, 0.0, 1, true),
            cp("west", -250.0, 0.0, 0, false),
            cp("east", 250.0, 0.0, 0, false),
            cp("carrier", 800.0, 0.0, 2, true),
        ]);
        let front = Front::new(&source, ModeKind::Breakthrough).unwrap();
        assert_eq!(front.attacker, 1);
        assert_eq!(front.stages, [vec![1], vec![2]]);
    }

    #[test]
    fn completes_and_keeps_hand_made_layouts() {
        let mut layouts = vec![karkand()];
        let mut own = generate(&karkand(), ModeKind::Rush).unwrap();
        own.generated = false;
        own.staged.as_mut().unwrap().tickets = 5.0;
        merge_layouts(&mut layouts, vec![own]);
        complete_layouts(&mut layouts);
        let modes: Vec<&str> = layouts.iter().map(|l| l.mode.as_str()).collect();
        assert_eq!(modes, [CONQUEST, RUSH, BREAKTHROUGH]);
        assert_eq!(layouts[1].staged.as_ref().unwrap().tickets, 5.0);
        // A hand-made layout based on another takes its points from it.
        let mut layouts = vec![karkand()];
        merge_layouts(
            &mut layouts,
            vec![GameModeDesc {
                mode: "rush".into(),
                size: 32,
                based_on: Some(LayoutRef { mode: CONQUEST.into(), size: 32 }),
                ..Default::default()
            }],
        );
        assert_eq!(layouts[1].mode, RUSH);
        assert_eq!(layouts[1].control_points.len(), 6);
    }

    /// Prints the layouts generated for the imported levels named in `LEVELS` (comma
    /// separated; all by default): `cargo test -p game_data generated_layouts -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore]
    fn generated_layouts() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../imported/levels");
        let wanted = std::env::var("LEVELS").ok();
        let mut names: Vec<String> = std::fs::read_dir(&root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| wanted.as_ref().is_none_or(|w| w.split(',').any(|x| x == n)))
            .collect();
        names.sort();
        for name in names {
            let Ok(level) = crate::read_ron::<crate::LevelDesc>(root.join(&name).join("level.ron")) else {
                continue;
            };
            let mut layouts = level.game_modes.clone();
            complete_layouts(&mut layouts);
            for layout in layouts.iter().filter(|l| l.generated) {
                let staged = layout.staged.as_ref().unwrap();
                let cp_name = |id: &str| {
                    layout.control_points.iter().find(|c| c.id == id).map_or(id.to_string(), |c| c.name.clone())
                };
                println!("{name} {} {}: attacker {}, {} tickets", layout.mode, layout.size, staged.attacker, staged.tickets);
                for stage in &staged.stages {
                    let charges: Vec<String> = stage
                        .charges
                        .iter()
                        .map(|c| format!("{} at {} ({:.0} {:.0})", c.name, cp_name(c.control_point.as_deref().unwrap_or("")), c.position[0], c.position[2]))
                        .collect();
                    let names = |ids: &[String]| ids.iter().map(|i| cp_name(i)).collect::<Vec<_>>().join(", ");
                    println!(
                        "    {}: points [{}] charges [{}] attackers [{}] defenders [{}]",
                        stage.name,
                        names(&stage.control_points),
                        charges.join("; "),
                        names(&stage.attacker_spawns),
                        names(&stage.defender_spawns)
                    );
                }
            }
        }
    }

    /// The example in docs/MODDING.md ("Game mode layouts").
    #[test]
    fn hand_made_layouts_parse() {
        let text = r#"(
            game_modes: [
                (
                    mode: "gpm_rush",
                    size: 32,
                    based_on: Some((mode: "gpm_cq", size: 32)),
                    staged: Some((
                        attacker: 2,
                        tickets: 120.0,
                        arm_seconds: 4.0,
                        defuse_seconds: 6.0,
                        fuse_seconds: 30.0,
                        stages: [
                            (
                                name: "The Hotel",
                                charges: [
                                    (name: "A", position: (-205.0, 156.0, -13.0), yaw: 180.0),
                                    (name: "B", position: (-185.0, 156.0, -16.0), yaw: 180.0,
                                     template: Some("woodencrate_destructible_tools")),
                                ],
                                attacker_spawns: ["305"],
                                defender_spawns: ["302", "306", "307"],
                            ),
                            // ... more stages
                        ],
                    )),
                ),
                (
                    mode: "gpm_breakthrough",
                    size: 32,
                    based_on: Some((mode: "gpm_cq", size: 32)),
                    staged: Some((
                        attacker: 2,
                        tickets: 170.0,
                        stages: [
                            (name: "Old Town", control_points: ["301", "302"]),
                            (name: "Market", control_points: ["306", "307"]),
                        ],
                    )),
                ),
            ],
        )"#;
        let layouts: ModeLayouts = ron::from_str(text).unwrap();
        let rush = layouts.game_modes[0].staged.as_ref().unwrap();
        assert_eq!(rush.stages[0].charges[1].template.as_deref(), Some("woodencrate_destructible_tools"));
        assert!(!rush.stages[0].charges[0].approximate);
        let bt = layouts.game_modes[1].staged.as_ref().unwrap();
        assert_eq!((bt.fuse_seconds, bt.stages[1].control_points.len()), (30.0, 2));
    }

    #[test]
    fn points_sharing_an_id_are_left_out() {
        let mut source = karkand();
        for x in 0..3 {
            source.control_points.push(cp("dup", 400.0 + x as f32, -300.0, 1, false));
        }
        let front = Front::new(&source, ModeKind::Rush).unwrap();
        assert!(front.stages.iter().flatten().all(|&i| source.control_points[i].id != "dup"));
        assert_eq!(front.stages.iter().flatten().count(), 5);
    }

    #[test]
    fn nothing_to_fight_over() {
        let source = layout(vec![cp("one", 0.0, 0.0, 1, true), cp("two", 0.0, 500.0, 2, true)]);
        assert!(generate(&source, ModeKind::Rush).is_none());
    }
}
