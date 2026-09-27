//! The game's own data formats.
//!
//! Nothing in here is BF2-specific: the importer converts BF2 data *into* these formats,
//! and new content can be authored directly in them. All files are RON so they can be
//! read and edited by hand. Coordinates are in the engine's convention: meters,
//! right-handed, +Y up, -Z forward.

use std::path::Path;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub mod effect;
pub mod level;
pub mod material;
pub mod object;
pub mod soldier;
pub mod weapon;

pub use effect::*;
pub use level::*;
pub use material::*;
pub use object::*;
pub use soldier::*;
pub use weapon::*;

#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: String,
        source: ron::error::SpannedError,
    },
    #[error("failed to serialize: {0}")]
    Serialize(#[from] ron::Error),
}

/// Reads a RON file into `T`.
pub fn read_ron<T: DeserializeOwned>(path: impl AsRef<Path>) -> Result<T, DataError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|source| DataError::Io {
        path: path.display().to_string(),
        source,
    })?;
    ron::from_str(&text).map_err(|source| DataError::Parse {
        path: path.display().to_string(),
        source,
    })
}

/// Writes `value` as pretty RON, creating parent directories as needed.
pub fn write_ron<T: Serialize>(path: impl AsRef<Path>, value: &T) -> Result<(), DataError> {
    let path = path.as_ref();
    let io_err = |source| DataError::Io {
        path: path.display().to_string(),
        source,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_err)?;
    }
    let text = ron::ser::to_string_pretty(value, ron::ser::PrettyConfig::default())?;
    std::fs::write(path, text).map_err(io_err)
}

/// A position + rotation (quaternion xyzw) + uniform-ish scale.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub position: [f32; 3],
    #[serde(default = "identity_quat")]
    pub rotation: [f32; 4],
    #[serde(default = "one_scale", skip_serializing_if = "is_one_scale")]
    pub scale: [f32; 3],
}

impl Default for Placement {
    fn default() -> Self {
        Self {
            position: [0.0; 3],
            rotation: identity_quat(),
            scale: one_scale(),
        }
    }
}

fn identity_quat() -> [f32; 4] {
    [0.0, 0.0, 0.0, 1.0]
}

fn one_scale() -> [f32; 3] {
    [1.0; 3]
}

fn is_one_scale(s: &[f32; 3]) -> bool {
    *s == one_scale()
}
