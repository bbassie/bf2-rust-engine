//! Temporary tuning knobs for soak experiments: `BF2_BOT_TUNE="name=value,..."`.

use std::{collections::HashMap, sync::OnceLock};

static KNOBS: OnceLock<HashMap<String, f32>> = OnceLock::new();

/// The knob `name`, or `default` when not set.
pub fn knob(name: &str, default: f32) -> f32 {
    KNOBS
        .get_or_init(|| {
            let spec = std::env::var("BF2_BOT_TUNE").unwrap_or_default();
            let knobs: HashMap<String, f32> = spec
                .split(',')
                .filter_map(|kv| kv.split_once('='))
                .filter_map(|(k, v)| Some((k.trim().to_string(), v.trim().parse().ok()?)))
                .collect();
            if !knobs.is_empty() {
                bevy::log::info!("bot tuning knobs: {knobs:?}");
            }
            knobs
        })
        .get(name)
        .copied()
        .unwrap_or(default)
}
