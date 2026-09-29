//! The vehicle HUD, in the game's own style (BF2 decides what it shows, not how it looks): a
//! panel bottom right with the vehicle and its hit points, who sits where, the speed and gear,
//! where the turret points, and the seat's guns and countermeasures; pilots also get flight
//! instruments around the crosshair: a horizon with a pitch ladder that banks with the
//! aircraft, the heading above, airspeed and throttle on the left, altitude and climb rate on
//! the right (the speed lit up in the jet's best turning band), and stall and pull-up
//! warnings. Markers over the view show where a jet is going (its flight path) and, while a
//! gunner's turret or gun is still turning after his aim, where the gun points.

use std::fmt::Write as _;

use avian3d::prelude::{SpatialQuery, SpatialQueryFilter};
use bevy::prelude::*;
use game_data::{JointInput, VehicleCategory};
use game_shared::{
    physics::GameLayer,
    vehicle::{Seated, VehicleData, VehicleHealth, VehicleState, VehicleWeapons},
};

use crate::{
    net::LocalSoldier,
    settings::{Action, Actions},
    ui_theme::{font, shadow},
    vehicles::{VehicleView, readable_name},
};

pub struct VehicleHudPlugin;

impl Plugin for VehicleHudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_hud)
            .add_systems(Update, (update_panel, update_instruments))
            .add_systems(
                PostUpdate,
                update_markers
                    .after(crate::camera::CameraSystems)
                    .before(bevy::ui::UiSystems::Layout),
            );
    }
}

const PANEL: Color = Color::srgba(0.05, 0.06, 0.08, 0.55);
const ACCENT: Color = Color::srgb(0.95, 0.75, 0.3);
const TEXT: Color = Color::srgb(0.92, 0.93, 0.95);
const DIM: Color = Color::srgba(0.85, 0.87, 0.9, 0.65);
/// Flight instrument lines and figures.
const INSTRUMENT: Color = Color::srgba(0.55, 1.0, 0.7, 0.9);
const WARNING: Color = Color::srgb(1.0, 0.35, 0.3);

/// Pitch ladder scale, pixels per degree, the ladder's rungs, and how far above and below the
/// horizon's current place rungs show.
const LADDER_SCALE: f32 = 6.0;
const LADDER_STEP: i32 = 10;
const LADDER_SHOWN: f32 = 24.0;
/// The instruments' half width and height around the screen centre.
const INSTRUMENT_HALF: Vec2 = Vec2::new(210.0, 150.0);

#[derive(Component)]
struct VehiclePanel;
#[derive(Component)]
struct PanelTitle;
/// The seat's role, after the vehicle's name in the title.
#[derive(Component)]
struct PanelRole;
#[derive(Component)]
struct PanelSeats;
/// One seat's marker, by seat index.
#[derive(Component)]
struct SeatPip(usize);
#[derive(Component)]
struct PanelHealthFill;
#[derive(Component)]
struct PanelSpeed;
#[derive(Component)]
struct PanelGuns;
#[derive(Component)]
struct TurretIndicator;
#[derive(Component)]
struct TurretLine;

#[derive(Component)]
struct Instruments;
/// Turns with the aircraft's roll; its child [`PitchLadder`] shifts with the pitch.
#[derive(Component)]
struct RollFrame;
#[derive(Component)]
struct PitchLadder;
/// A rung of the pitch ladder (its line or a label), at this many degrees.
#[derive(Component)]
struct Rung(f32);
#[derive(Component)]
struct HeadingText;
#[derive(Component)]
struct SpeedText;
#[derive(Component)]
struct AltitudeText;
#[derive(Component)]
struct ThrottleFill;
#[derive(Component)]
struct BoostFill;
#[derive(Component)]
struct WarningText;
/// Where a jet is going.
#[derive(Component)]
struct FlightPathMarker;
/// Where a gunner's gun points while it catches up with his aim.
#[derive(Component)]
struct GunMarker;

/// The speed shows in this colour within this share of a jet's corner speed.
const CORNER_BAND: f32 = 0.12;
const CORNER_COLOR: Color = Color::srgb(0.45, 0.8, 1.0);
/// The gun marker shows once it's this far (logical pixels) from the crosshair.
const GUN_MARKER_MIN_OFFSET: f32 = 10.0;


fn bar(width: f32, height: f32) -> (Node, BackgroundColor) {
    (
        Node {
            width: px(width),
            height: px(height),
            border_radius: BorderRadius::all(px(height * 0.5)),
            ..default()
        },
        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.15)),
    )
}

fn fill(color: Color) -> (Node, BackgroundColor) {
    (
        Node {
            width: percent(100),
            height: percent(100),
            border_radius: BorderRadius::all(px(3)),
            ..default()
        },
        BackgroundColor(color),
    )
}

/// A ring marker over the view, placed by `update_markers`.
fn ring(size: f32, color: Color) -> (Node, BorderColor, Visibility, Pickable) {
    (
        Node {
            position_type: PositionType::Absolute,
            width: px(size),
            height: px(size),
            border: UiRect::all(px(2)),
            border_radius: BorderRadius::MAX,
            ..default()
        },
        BorderColor::all(color),
        Visibility::Hidden,
        Pickable::IGNORE,
    )
}

fn spawn_hud(mut commands: Commands) {
    commands.spawn((FlightPathMarker, ring(16.0, INSTRUMENT)));
    commands.spawn((GunMarker, ring(22.0, Color::srgba(1.0, 0.85, 0.4, 0.85))));
    // The panel, where soldiers have their weapon and ammo.
    commands
        .spawn((
            VehiclePanel,
            Node {
                position_type: PositionType::Absolute,
                right: px(24),
                bottom: px(24),
                padding: UiRect::axes(px(14), px(10)),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::End,
                row_gap: px(6),
                border_radius: BorderRadius::all(px(8)),
                ..default()
            },
            BackgroundColor(PANEL),
            Visibility::Hidden,
        ))
        .with_children(|panel| {
            panel
                .spawn(Node {
                    column_gap: px(12),
                    align_items: AlignItems::Center,
                    ..default()
                })
                .with_children(|row| {
                    // Hull and turret seen from above, the turret line turning with the aim.
                    row.spawn((
                        TurretIndicator,
                        Node {
                            width: px(22),
                            height: px(30),
                            border: UiRect::all(px(2)),
                            border_radius: BorderRadius::all(px(4)),
                            justify_content: JustifyContent::Center,
                            ..default()
                        },
                        BorderColor::all(DIM),
                        Visibility::Hidden,
                    ))
                    .with_child((
                        Node {
                            position_type: PositionType::Absolute,
                            top: px(4),
                            width: px(18),
                            height: px(18),
                            justify_content: JustifyContent::Center,
                            ..default()
                        },
                        TurretLine,
                        UiTransform::default(),
                    ))
                    .with_children(|turret| {
                        turret.spawn((
                            Node {
                                position_type: PositionType::Absolute,
                                top: px(-8),
                                width: px(3),
                                height: px(17),
                                border_radius: BorderRadius::all(px(2)),
                                ..default()
                            },
                            BackgroundColor(ACCENT),
                        ));
                    });
                    row.spawn((PanelTitle, Text::new(""), font(14.0), TextColor(ACCENT)))
                        .with_child((PanelRole, TextSpan::new(""), font(14.0), TextColor(DIM)));
                });
            panel.spawn((
                PanelSeats,
                Node {
                    column_gap: px(4),
                    ..default()
                },
            ));
            panel.spawn(bar(200.0, 6.0)).with_child((PanelHealthFill, fill(Color::srgb(0.45, 0.85, 0.5))));
            panel.spawn((PanelSpeed, Text::new(""), font(26.0), TextColor(TEXT)));
            panel.spawn((PanelGuns, Text::new(""), font(14.0), TextColor(TEXT), TextLayout::justify(Justify::Right)));
        });

    // Flight instruments around the screen centre.
    commands
        .spawn((
            Instruments,
            Node {
                position_type: PositionType::Absolute,
                left: percent(50),
                top: percent(50),
                ..default()
            },
            Visibility::Hidden,
        ))
        .with_children(|centre| {
            // The horizon and pitch ladder around the crosshair.
            centre
                .spawn(Node {
                    position_type: PositionType::Absolute,
                    left: px(-INSTRUMENT_HALF.y),
                    top: px(-INSTRUMENT_HALF.y),
                    width: px(INSTRUMENT_HALF.y * 2.0),
                    height: px(INSTRUMENT_HALF.y * 2.0),
                    ..default()
                })
                .with_children(|window| {
                    window
                        .spawn((
                            RollFrame,
                            Node {
                                position_type: PositionType::Absolute,
                                width: percent(100),
                                height: percent(100),
                                ..default()
                            },
                            UiTransform::default(),
                        ))
                        .with_children(|frame| {
                            frame
                                .spawn((
                                    PitchLadder,
                                    Node {
                                        position_type: PositionType::Absolute,
                                        left: px(INSTRUMENT_HALF.y),
                                        top: px(INSTRUMENT_HALF.y),
                                        ..default()
                                    },
                                ))
                                .with_children(|ladder| {
                                    for step in (-90 / LADDER_STEP)..=(90 / LADDER_STEP) {
                                        let degrees = step * LADDER_STEP;
                                        let y = -(degrees as f32) * LADDER_SCALE;
                                        let horizon = degrees == 0;
                                        let width = if horizon { 260.0 } else { 110.0 };
                                        ladder.spawn((
                                            Rung(degrees as f32),
                                            Node {
                                                position_type: PositionType::Absolute,
                                                left: px(-width * 0.5),
                                                top: px(y - 1.0),
                                                width: px(width),
                                                height: px(if horizon { 2.0 } else { 1.5 }),
                                                ..default()
                                            },
                                            BackgroundColor(INSTRUMENT.with_alpha(if degrees < 0 { 0.55 } else { 0.9 })),
                                        ));
                                        if !horizon {
                                            for side in [-1.0f32, 1.0] {
                                                ladder.spawn((
                                                    Rung(degrees as f32),
                                                    Text::new(format!("{}", degrees.abs())),
                                                    font(11.0),
                                                    TextColor(INSTRUMENT),
                                                    TextLayout::no_wrap(),
                                                    Node {
                                                        position_type: PositionType::Absolute,
                                                        left: px(side * (width * 0.5 + 16.0) - 8.0),
                                                        top: px(y - 7.0),
                                                        ..default()
                                                    },
                                                ));
                                            }
                                        }
                                    }
                                });
                        });
                });
            centre.spawn((
                HeadingText,
                Text::new(""),
                font(16.0),
                TextColor(INSTRUMENT),
                shadow(),
                TextLayout::justify(Justify::Center),
                Node {
                    position_type: PositionType::Absolute,
                    left: px(-40.0),
                    top: px(-INSTRUMENT_HALF.y - 26.0),
                    width: px(80.0),
                    justify_content: JustifyContent::Center,
                    ..default()
                },
            ));
            // Airspeed and throttle on the left, altitude and climb on the right.
            centre
                .spawn(Node {
                    position_type: PositionType::Absolute,
                    left: px(-INSTRUMENT_HALF.x - 90.0),
                    top: px(-24.0),
                    width: px(90.0),
                    column_gap: px(8),
                    align_items: AlignItems::Center,
                    ..default()
                })
                .with_children(|left| {
                    left.spawn((SpeedText, Text::new(""), font(16.0), TextColor(INSTRUMENT), shadow(), TextLayout::no_wrap()));
                    left.spawn(Node {
                        flex_direction: FlexDirection::ColumnReverse,
                        column_gap: px(3),
                        ..default()
                    })
                    .with_children(|bars| {
                        bars.spawn((
                            Node {
                                width: px(6),
                                height: px(60),
                                flex_direction: FlexDirection::ColumnReverse,
                                border_radius: BorderRadius::all(px(3)),
                                ..default()
                            },
                            BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.15)),
                        ))
                        .with_child((
                            ThrottleFill,
                            Node {
                                width: percent(100),
                                height: percent(50),
                                border_radius: BorderRadius::all(px(3)),
                                ..default()
                            },
                            BackgroundColor(INSTRUMENT),
                        ));
                    });
                    left.spawn((
                        Node {
                            width: px(4),
                            height: px(60),
                            flex_direction: FlexDirection::ColumnReverse,
                            border_radius: BorderRadius::all(px(2)),
                            ..default()
                        },
                        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.1)),
                    ))
                    .with_child((
                        BoostFill,
                        Node {
                            width: percent(100),
                            height: percent(100),
                            border_radius: BorderRadius::all(px(2)),
                            ..default()
                        },
                        BackgroundColor(ACCENT),
                    ));
                });
            centre.spawn((
                AltitudeText,
                Text::new(""),
                font(16.0),
                TextColor(INSTRUMENT),
                shadow(),
                TextLayout::no_wrap(),
                Node {
                    position_type: PositionType::Absolute,
                    left: px(INSTRUMENT_HALF.x + 12.0),
                    top: px(-20.0),
                    ..default()
                },
            ));
            centre.spawn((
                WarningText,
                Text::new(""),
                font(22.0),
                TextColor(WARNING),
                shadow(),
                TextLayout::justify(Justify::Center),
                Node {
                    position_type: PositionType::Absolute,
                    left: px(-120.0),
                    top: px(INSTRUMENT_HALF.y * 0.55),
                    width: px(240.0),
                    justify_content: JustifyContent::Center,
                    ..default()
                },
            ));
        });
}

type PanelNodes<'w, 's> = (
    Single<'w, 's, (&'static mut Visibility, &'static Children), With<VehiclePanel>>,
    Single<'w, 's, &'static mut Text, (With<PanelTitle>, Without<PanelSpeed>, Without<PanelGuns>)>,
    Single<'w, 's, &'static mut Text, (With<PanelSpeed>, Without<PanelTitle>, Without<PanelGuns>)>,
    Single<'w, 's, &'static mut Text, (With<PanelGuns>, Without<PanelTitle>, Without<PanelSpeed>)>,
    Single<'w, 's, (&'static mut Node, &'static mut BackgroundColor), (With<PanelHealthFill>, Without<SeatPip>)>,
);

/// The panel: vehicle, seat and role, the seats (ours outlined, taken ones filled), hit
/// points, speed (and gear), the turret's direction and the seat's guns.
#[allow(clippy::too_many_arguments)]
fn update_panel(
    mut commands: Commands,
    actions: Actions,
    seated: Query<&Seated, With<LocalSoldier>>,
    riders: Query<&Seated>,
    vehicles: Query<(
        &VehicleView,
        &VehicleData,
        Option<&VehicleHealth>,
        Option<&VehicleWeapons>,
        Option<&VehicleState>,
    )>,
    nodes: PanelNodes,
    seats: Single<(Entity, Option<&Children>), With<PanelSeats>>,
    mut pips: Query<(&SeatPip, &mut BackgroundColor, &mut BorderColor), Without<PanelHealthFill>>,
    mut turret: Single<&mut Visibility, (With<TurretIndicator>, Without<VehiclePanel>)>,
    mut turret_line: Single<&mut UiTransform, With<TurretLine>>,
    mut role_text: Single<&mut TextSpan, With<PanelRole>>,
) {
    let (mut panel, _) = nodes.0.into_inner();
    let (mut title, mut speed_text, mut guns_text, (mut health_node, mut health_color)) =
        (nodes.1, nodes.2, nodes.3, nodes.4.into_inner());
    let Some((seated, (view, data, health, weapons, state))) = seated
        .single()
        .ok()
        .and_then(|s| vehicles.get(s.vehicle).ok().map(|v| (s, v)))
    else {
        panel.set_if_neq(Visibility::Hidden);
        return;
    };
    panel.set_if_neq(Visibility::Inherited);
    let model = &data.0;
    let desc = &model.desc;
    let seat = seated.seat as usize;
    let role = match seat {
        0 if desc.category.flies() => "PILOT",
        0 if desc.category == VehicleCategory::Stationary => "GUNNER",
        0 => "DRIVER",
        _ if model.seat_aims(seat) => "GUNNER",
        _ => "PASSENGER",
    };
    let name = desc.display_name.to_uppercase();
    if title.0 != name {
        title.0 = name;
    }
    let role = format!("   {role}");
    if role_text.0 != role {
        role_text.0 = role;
    }

    // Seats: one pip each, filled while taken, ours outlined.
    let (seats_node, children) = seats.into_inner();
    if children.map_or(0, |c| c.len()) != desc.seats.len() {
        commands.entity(seats_node).despawn_children();
        for index in 0..desc.seats.len() {
            commands.spawn((
                SeatPip(index),
                Node {
                    width: px(12),
                    height: px(12),
                    border: UiRect::all(px(2)),
                    border_radius: BorderRadius::all(px(3)),
                    ..default()
                },
                BackgroundColor(Color::NONE),
                BorderColor::all(DIM),
                ChildOf(seats_node),
            ));
        }
    }
    for (pip, mut background, mut border) in &mut pips {
        let taken = riders.iter().any(|r| r.vehicle == seated.vehicle && r.seat as usize == pip.0);
        let ours = pip.0 == seat;
        background.0 = if taken { if ours { ACCENT } else { TEXT } } else { Color::NONE };
        *border = BorderColor::all(if ours { ACCENT } else { DIM });
    }

    let fraction = health.map_or(1.0, |h| (h.current / h.max.max(1.0)).clamp(0.0, 1.0));
    crate::hud::set_width(&mut health_node, percent(fraction * 100.0));
    health_color.set_if_neq(BackgroundColor(Color::srgb(0.95 - 0.5 * fraction, 0.35 + 0.5 * fraction, 0.35)));

    let mut speed = format!("{:.0} km/h", view.velocity.length() * 3.6);
    if desc.engine.gearbox.is_some() && seat == 0 {
        let gear = state.map_or(0, |s| s.gear);
        let _ = write!(speed, "  {}", if gear < 0 { "R".to_string() } else { format!("G{}", gear + 1) });
    }
    if speed_text.0 != speed {
        speed_text.0 = speed;
    }

    // The turret this seat aims, relative to the hull.
    let aimed = desc.parts.iter().enumerate().find_map(|(i, part)| {
        let joint = part.joint.as_ref().filter(|j| j.seat as usize == seat)?;
        joint.axes[0].input.filter(|input| *input == JointInput::AimYaw)?;
        let angles = view.joints.get(model.joint_index[i]?)?;
        Some(angles[0])
    });
    turret.set_if_neq(if aimed.is_some() && !desc.category.flies() { Visibility::Inherited } else { Visibility::Hidden });
    if let Some(yaw) = aimed {
        turret_line.rotation = Rot2::radians(-yaw);
    }

    // The guns this seat fires.
    let mut guns = String::new();
    for (i, (w, gun)) in desc.weapons.iter().zip(&model.guns).enumerate().filter(|(_, (w, _))| w.seat as usize == seat) {
        let status = weapons.and_then(|s| s.guns.get(i)).copied().unwrap_or_default();
        // Unlocalized names are template names.
        let unnamed = gun.display_name.is_empty() || gun.display_name == desc.display_name || gun.display_name.contains('_');
        let name = if unnamed { readable_name(&gun.name, &desc.name) } else { gun.display_name.clone() };
        let trigger = match (&w.countermeasure, w.alt_fire) {
            (Some(_), _) => format!("[{}] ", actions.label(Action::Countermeasures)),
            (None, true) => "[2] ".into(),
            (None, false) => String::new(),
        };
        let chosen = status.selected && w.countermeasure.is_none();
        if !guns.is_empty() {
            guns.push('\n');
        }
        let _ = write!(guns, "{}{trigger}{}", if chosen { "> " } else { "" }, name.to_uppercase());
        let _ = match (status.reloading, status.rounds) {
            (true, _) => write!(guns, "   reloading"),
            (false, u16::MAX) => Ok(()),
            (false, rounds) => write!(guns, "   {rounds}"),
        };
        if gun.fire.overheat.is_some() {
            let _ = match status.heat {
                255 => write!(guns, "   OVERHEATED"),
                heat => write!(guns, "   heat {:.0}%", heat as f32 / 2.54),
            };
        }
        if gun.fire.lock.is_some() {
            let _ = match status.lock {
                255 => write!(guns, "   LOCKED"),
                0 => Ok(()),
                lock => write!(guns, "   locking {:.0}%", lock as f32 / 2.55),
            };
        }
    }
    if guns_text.0 != guns {
        guns_text.0 = guns;
    }
}

type InstrumentTexts<'w, 's> = (
    Single<'w, 's, &'static mut Text, (With<HeadingText>, Without<SpeedText>, Without<AltitudeText>, Without<WarningText>)>,
    Single<'w, 's, &'static mut Text, (With<SpeedText>, Without<HeadingText>, Without<AltitudeText>, Without<WarningText>)>,
    Single<'w, 's, &'static mut Text, (With<AltitudeText>, Without<HeadingText>, Without<SpeedText>, Without<WarningText>)>,
    Single<'w, 's, (&'static mut Text, &'static mut TextColor), (With<WarningText>, Without<HeadingText>, Without<SpeedText>, Without<AltitudeText>)>,
);

/// Pilots' flight instruments, from the aircraft as drawn.
#[allow(clippy::too_many_arguments)]
fn update_instruments(
    seated: Query<&Seated, With<LocalSoldier>>,
    vehicles: Query<(&VehicleView, &VehicleData)>,
    spatial: SpatialQuery,
    mut root: Single<&mut Visibility, With<Instruments>>,
    mut roll_frame: Single<&mut UiTransform, With<RollFrame>>,
    mut ladder: Single<&mut Node, (With<PitchLadder>, Without<ThrottleFill>, Without<BoostFill>)>,
    mut throttle: Single<&mut Node, (With<ThrottleFill>, Without<PitchLadder>, Without<BoostFill>)>,
    mut boost: Single<&mut Node, (With<BoostFill>, Without<PitchLadder>, Without<ThrottleFill>)>,
    mut rungs: Query<(&Rung, &mut Visibility), Without<Instruments>>,
    texts: InstrumentTexts,
    mut speed_color: Single<&mut TextColor, (With<SpeedText>, Without<WarningText>)>,
) {
    let (mut heading_text, mut speed_text, mut altitude_text, warning) = texts;
    let (mut warning_text, mut warning_color) = warning.into_inner();
    let flying = seated
        .single()
        .ok()
        .filter(|s| s.seat == 0)
        .and_then(|s| vehicles.get(s.vehicle).ok())
        .filter(|(_, data)| data.0.desc.category.flies());
    let Some((view, data)) = flying else {
        root.set_if_neq(Visibility::Hidden);
        return;
    };
    root.set_if_neq(Visibility::Inherited);
    let desc = &data.0.desc;
    let rotation = view.transform.rotation;
    let forward = rotation * Vec3::NEG_Z;
    let right = rotation * Vec3::X;
    let pitch = forward.y.clamp(-1.0, 1.0).asin();
    // Banked right, the right wing dips and the horizon turns the other way on screen.
    let roll = (-right.y).clamp(-1.0, 1.0).asin();
    let roll = if (rotation * Vec3::Y).y < 0.0 { std::f32::consts::PI - roll } else { roll };
    roll_frame.rotation = Rot2::radians(-roll);
    let top = px(INSTRUMENT_HALF.y + pitch.to_degrees() * LADDER_SCALE);
    if ladder.top != top {
        ladder.top = top;
    }
    for (rung, mut visibility) in &mut rungs {
        let near = (rung.0 - pitch.to_degrees()).abs() <= LADDER_SHOWN;
        visibility.set_if_neq(if near { Visibility::Inherited } else { Visibility::Hidden });
    }

    let heading = (-crate::vehicles::heading(rotation).to_degrees()).rem_euclid(360.0);
    let wanted = format!("{:03.0}", heading.round() % 360.0);
    if heading_text.0 != wanted {
        heading_text.0 = wanted;
    }
    let speed = view.velocity.length();
    let wanted = format!("{:.0}\nkm/h", speed * 3.6);
    if speed_text.0 != wanted {
        speed_text.0 = wanted;
    }
    // Jets turn best around their corner speed.
    let corner = game_shared::flight::JetEnvelope::of(desc).corner;
    let in_band = desc.category == VehicleCategory::Air && (speed / corner - 1.0).abs() < CORNER_BAND;
    speed_color.0 = if in_band { CORNER_COLOR } else { INSTRUMENT };
    let filter = SpatialQueryFilter::from_mask(GameLayer::World);
    let altitude = spatial
        .cast_ray(view.transform.translation, Dir3::NEG_Y, 2000.0, true, &filter)
        .map_or(2000.0, |hit| hit.distance);
    let climb = view.velocity.y;
    let wanted = format!("{altitude:.0} m\n{}{climb:.1} m/s", if climb >= 0.0 { "+" } else { "" });
    if altitude_text.0 != wanted {
        altitude_text.0 = wanted;
    }
    let throttle_height = percent(view.engine.clamp(0.0, 1.0) * 100.0);
    if throttle.height != throttle_height {
        throttle.height = throttle_height;
    }
    let boost_height = percent(if desc.afterburner.is_some() { view.boost.clamp(0.0, 1.0) * 100.0 } else { 0.0 });
    if boost.height != boost_height {
        boost.height = boost_height;
    }

    // Warnings: the ground coming up fast, or too slow to fly (jump jets that slow hover).
    let impact = if climb < -1.0 { altitude / -climb } else { f32::MAX };
    let slow = desc.category == VehicleCategory::Air && altitude > 8.0 && speed < 30.0;
    let body = game_shared::flight::BodyState {
        position: view.transform.translation,
        rotation,
        velocity: view.velocity,
        angular_velocity: Vec3::ZERO,
    };
    let stalled = game_shared::flight::jet_airborne(altitude) && game_shared::flight::jet_stall(desc, &body) > 0.35;
    let (warning, color) = if altitude < 200.0 && impact < 4.0 {
        ("PULL UP", WARNING)
    } else if slow && desc.rotor.is_some() {
        ("HOVER", INSTRUMENT)
    } else if stalled {
        ("STALL", WARNING)
    } else {
        ("", WARNING)
    };
    if warning_text.0 != warning {
        warning_text.0 = warning.into();
    }
    warning_color.0 = color;
}

/// Places the flight path and gun markers over the view.
#[allow(clippy::type_complexity)]
fn update_markers(
    seated: Query<&Seated, With<LocalSoldier>>,
    vehicles: Query<(&VehicleView, &VehicleData)>,
    camera: Single<(&Camera, &Transform), With<crate::camera::PlayerCamera>>,
    spatial: SpatialQuery,
    mut path: Single<(&mut Node, &mut Visibility), (With<FlightPathMarker>, Without<GunMarker>)>,
    mut gun: Single<(&mut Node, &mut Visibility), (With<GunMarker>, Without<FlightPathMarker>)>,
) {
    let (camera, eye) = *camera;
    let eye_global = GlobalTransform::from(*eye);
    let centre = camera.logical_viewport_size().unwrap_or_default() * 0.5;
    let place = |node: &mut Node, visibility: &mut Visibility, point: Option<Vec3>, size: f32, min_offset: f32| {
        let at = point
            .filter(|p| (*p - eye.translation).dot(eye.forward().as_vec3()) > 0.0)
            .and_then(|p| camera.world_to_viewport(&eye_global, p).ok())
            .filter(|at| at.distance(centre) >= min_offset);
        match at {
            Some(at) => {
                node.left = px(at.x - size * 0.5);
                node.top = px(at.y - size * 0.5);
                *visibility = Visibility::Inherited;
            }
            None => *visibility = Visibility::Hidden,
        }
    };
    let inside = seated.single().ok().and_then(|s| vehicles.get(s.vehicle).ok().map(|(v, d)| (s, v, d)));
    // The flight path: a jet's velocity, far ahead.
    let flight_path = inside
        .filter(|(s, _, d)| s.seat == 0 && d.0.desc.category == VehicleCategory::Air)
        .filter(|(_, v, _)| v.velocity.length() > 20.0)
        .map(|(_, v, _)| eye.translation + v.velocity.normalize() * 1000.0);
    let (node, visibility) = &mut *path;
    place(node, visibility, flight_path, 16.0, 0.0);
    // The gun: where the seat's main gun points, while it's off the aim.
    let gun_point = inside.filter(|(s, _, d)| d.0.seat_aims(s.seat as usize)).and_then(|(s, view, data)| {
        let model = &data.0;
        let index = model
            .desc
            .weapons
            .iter()
            .enumerate()
            .filter(|(_, w)| w.seat == s.seat as u32)
            .min_by_key(|(_, w)| w.alt_fire)
            .map(|(i, _)| i)?;
        let transforms = model.part_transforms(&view.joints);
        let muzzle = view.transform * model.muzzle(&transforms, index);
        // Where the barrel's line meets something, like the aim converges on what's under
        // the crosshair.
        let direction = Dir3::new(muzzle.rotation * Vec3::NEG_Z).ok()?;
        let filter = SpatialQueryFilter::from_mask([GameLayer::World, GameLayer::Vehicle]).with_excluded_entities([s.vehicle]);
        let distance = spatial.cast_ray(muzzle.translation, direction, 800.0, true, &filter).map_or(800.0, |hit| hit.distance);
        Some(muzzle.translation + *direction * distance)
    });
    let (node, visibility) = &mut *gun;
    place(node, visibility, gun_point, 22.0, GUN_MARKER_MIN_OFFSET);
}
