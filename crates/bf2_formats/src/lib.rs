//! Readers for Battlefield 2 data formats.
//!
//! This crate only *reads* files from a user's own BF2 installation. It has no engine
//! dependency so it can be used by the importer, tools and tests alike.

pub mod anim;
pub mod collision;
pub mod con;
pub mod install;
pub mod localization;
pub mod mesh;
pub mod reader;
pub mod road;
pub mod terrain;
pub mod vfs;

pub use install::{Bf2Install, InstallError, LevelInfo, Side};
pub use vfs::Vfs;
