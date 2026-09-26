//! Readers for Battlefield 2 data formats.
//!
//! This crate only *reads* files from a user's own BF2 installation. It has no engine
//! dependency so it can be used by the importer, tools and tests alike.

pub mod collision;
pub mod con;
pub mod install;
pub mod mesh;
pub mod reader;
pub mod vfs;

pub use install::{Bf2Install, LevelInfo, Side};
pub use vfs::Vfs;
