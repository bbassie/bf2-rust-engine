//! Checks for values that come over the network, applied where they are received (the trust
//! boundary): finite numbers in range, bounded lengths and names that are safe to use as a
//! file name. A modified client or a malicious server can send anything the wire format can
//! carry; everything here turns that into either a sane value or a refusal.

use std::f32::consts::{FRAC_PI_2, TAU};

use bevy::prelude::*;

use crate::{
    chat::clean_text,
    input::{INPUT_REDUNDANCY, InputFrame},
};

/// Most input frames the server reads from one [`crate::input::InputPacket`] (the newest
/// ones). Clients send [`INPUT_REDUNDANCY`]; the rest is room for a debug input delay.
pub const MAX_INPUT_FRAMES: usize = INPUT_REDUNDANCY * 4;

/// Longest player name, in characters.
pub const MAX_NAME_LENGTH: usize = 24;

/// Longest name accepted as a file name component (level, vehicle and object templates).
pub const MAX_PATH_COMPONENT: usize = 96;

/// An input frame as the server may use it: `None` if its view angles aren't finite (a NaN
/// yaw would turn the soldier's position into NaN, which replicates to everyone and makes the
/// soldier impossible to hit). The yaw is wrapped into `0..TAU` (turning is the same, and the
/// value stays precise) and the pitch clamped to straight up or down.
pub fn input_frame(mut frame: InputFrame) -> Option<InputFrame> {
    if !frame.yaw.is_finite() || !frame.pitch.is_finite() {
        return None;
    }
    frame.yaw = frame.yaw.rem_euclid(TAU);
    // `rem_euclid` of a tiny negative number can round up to TAU itself.
    if frame.yaw >= TAU {
        frame.yaw = 0.0;
    }
    frame.pitch = frame.pitch.clamp(-FRAC_PI_2, FRAC_PI_2);
    Some(frame)
}

/// A point with finite coordinates, or `None`.
pub fn finite_point(point: Vec3) -> Option<Vec3> {
    point.is_finite().then_some(point)
}

/// A player name as a client asked for it, cleaned (printable ASCII, at most
/// [`MAX_NAME_LENGTH`] characters, trimmed). `None` if nothing is left.
pub fn player_name(requested: &str) -> Option<String> {
    let name = clean_text(requested, MAX_NAME_LENGTH);
    (!name.is_empty()).then_some(name)
}

/// `wanted`, or if `taken` says someone already has it (names compare case-insensitively),
/// the first free `wanted (2)`, `wanted (3)`, ... cut to fit [`MAX_NAME_LENGTH`].
pub fn unique_name(wanted: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(wanted) {
        return wanted.to_string();
    }
    // Always ends: there are only so many players.
    let mut n = 2u32;
    loop {
        let suffix = format!(" ({n})");
        let keep = MAX_NAME_LENGTH.saturating_sub(suffix.len());
        let base: String = wanted.chars().take(keep).collect();
        let candidate = format!("{}{suffix}", base.trim_end());
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Whether `name` can be used as one component of a file path (`levels/<name>/`,
/// `vehicles/<name>.ron`): not empty, not too long, only letters, digits and `_ - . +`, no
/// leading dot (so no `..`), no trailing dot or space, and not a Windows device name (`CON`,
/// `NUL`, `COM1`, ...). Names received from a server go through this before they touch the
/// file system.
pub fn path_component(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_PATH_COMPONENT || name.starts_with('.') || name.ends_with(['.', ' ']) {
        return false;
    }
    if name.contains("..") {
        return false;
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | ' ')) {
        return false;
    }
    let stem = name.split('.').next().unwrap_or(name).trim_end().to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    !device
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_with_nan_or_infinite_angles_are_dropped() {
        let frame = InputFrame {
            yaw: 1.0,
            pitch: 0.5,
            ..default()
        };
        assert_eq!(input_frame(frame), Some(frame));
        assert_eq!(input_frame(InputFrame { yaw: f32::NAN, ..frame }), None);
        assert_eq!(input_frame(InputFrame { pitch: f32::NAN, ..frame }), None);
        assert_eq!(input_frame(InputFrame { yaw: f32::INFINITY, ..frame }), None);
        assert_eq!(input_frame(InputFrame { pitch: f32::NEG_INFINITY, ..frame }), None);
    }

    #[test]
    fn yaw_is_wrapped_and_pitch_clamped() {
        let frame = input_frame(InputFrame {
            yaw: -0.5 - 3.0 * TAU,
            pitch: 7.0,
            ..default()
        })
        .unwrap();
        assert!((frame.yaw - (TAU - 0.5)).abs() < 1e-3, "{}", frame.yaw);
        assert_eq!(frame.pitch, FRAC_PI_2);
        let tiny = input_frame(InputFrame { yaw: -1e-9, ..default() }).unwrap();
        assert!((0.0..TAU).contains(&tiny.yaw));
        // The same direction either way.
        let raw = Quat::from_rotation_y(-0.5 - 3.0 * TAU) * Vec3::NEG_Z;
        let wrapped = Quat::from_rotation_y(frame.yaw) * Vec3::NEG_Z;
        assert!(raw.distance(wrapped) < 1e-3);
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(player_name("  Bob  ").as_deref(), Some("Bob"));
        assert_eq!(player_name("   "), None);
        assert_eq!(player_name("\u{202e}evil\n").as_deref(), Some("?evil?"));
        assert_eq!(player_name(&"x".repeat(100)).map(|n| n.len()), Some(MAX_NAME_LENGTH));
    }

    #[test]
    fn a_name_clash_is_resolved() {
        let names = ["Bob", "bob (2)", "Alice"];
        let taken = |n: &str| names.iter().any(|t| t.eq_ignore_ascii_case(n));
        assert_eq!(unique_name("Carol", taken), "Carol");
        assert_eq!(unique_name("BOB", taken), "BOB (3)");
        assert_eq!(unique_name("alice", taken), "alice (2)");
        // Long names are cut to make room for the number.
        let long = "x".repeat(MAX_NAME_LENGTH);
        let taken = |n: &str| n == long;
        let unique = unique_name(&long, taken);
        assert!(unique.len() <= MAX_NAME_LENGTH && unique.ends_with(" (2)"), "{unique}");
    }

    #[test]
    fn path_components_reject_bad_names() {
        for good in ["strike_at_karkand", "usapc_lav25", "ch_jet_su30", "AIX-Map 2", "c4.v2", "mod+x"] {
            assert!(path_component(good), "{good}");
        }
        for bad in [
            "",
            "..",
            ".",
            "../imported",
            "a/../b",
            "a/b",
            "a\\b",
            "C:",
            "C:\\Windows",
            "/etc/passwd",
            ".hidden",
            "trailing.",
            "trailing ",
            "a..b",
            "nul",
            "CON.ron",
            "com1",
            "Lpt9.x",
            "tab\there",
            "new\nline",
            "ünï",
        ] {
            assert!(!path_component(bad), "{bad:?}");
        }
        assert!(!path_component(&"a".repeat(MAX_PATH_COMPONENT + 1)));
        assert!(path_component("computer"), "only the exact device names");
    }

    #[test]
    fn points_must_be_finite() {
        assert_eq!(finite_point(Vec3::ONE), Some(Vec3::ONE));
        assert_eq!(finite_point(Vec3::new(f32::NAN, 0.0, 0.0)), None);
        assert_eq!(finite_point(Vec3::new(0.0, f32::INFINITY, 0.0)), None);
    }
}
