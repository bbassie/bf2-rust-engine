//! Mods: folders laid out like `imported/` whose files add to or replace the imported ones
//! (see `docs/MODDING.md`). Every subfolder of the mods folder with a `mod.ron` is a mod.
//!
//! ```text
//! mods/
//!   my_mod/
//!     mod.ron                         (name: "My mod", priority: 0)
//!     levels/my_level/level.ron       a new level
//!     weapons/usrif_m4.ron            replaces the imported M4 completely
//!     weapons/usrif_m4.patch.ron      or changes only some of its fields
//!     weapons/my_rifle.patch.ron      a new weapon made from another: (base: "usrif_m4", ...)
//! ```
//!
//! [`GamePaths`](crate::config::GamePaths) looks in every enabled mod (highest priority
//! first) before `imported/`. The client's `imported://` asset source does the same.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// A mod's `mod.ron`.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ModInfo {
    pub name: String,
    pub description: String,
    /// Off: the folder is ignored.
    pub enabled: bool,
    /// Higher wins when mods have the same file.
    pub priority: i32,
}

impl Default for ModInfo {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            enabled: true,
            priority: 0,
        }
    }
}

/// A mod found in the mods folder.
#[derive(Clone, Debug)]
pub struct Mod {
    pub dir: PathBuf,
    pub info: ModInfo,
}

/// The enabled mods in `dir`, highest priority first (then by folder name).
pub fn discover(dir: &Path) -> Vec<Mod> {
    let mut mods: Vec<Mod> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            let text = std::fs::read_to_string(dir.join("mod.ron")).ok()?;
            let mut info: ModInfo = match ron::from_str(&text) {
                Ok(info) => info,
                Err(err) => {
                    bevy::log::warn!("{}: {err}", dir.join("mod.ron").display());
                    return None;
                }
            };
            if info.name.is_empty() {
                info.name = entry.file_name().to_string_lossy().into_owned();
            }
            info.enabled.then_some(Mod { dir, info })
        })
        .collect();
    mods.sort_by(|a, b| b.info.priority.cmp(&a.info.priority).then(a.dir.cmp(&b.dir)));
    mods
}

/// Where a patch file for `path` (`x.ron`) would be: `x.patch.ron`.
pub fn patch_path(path: &Path) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{stem}.patch.ron"))
}

/// Reads a RON file found in `roots` (highest priority first), with the patches of the roots
/// above it applied. A patch that names a `base` (a file name in the same folder, without
/// `.ron`) makes a new file from that one.
pub fn read_layered<T: Serialize + DeserializeOwned>(roots: &[&Path], relative: &Path) -> anyhow::Result<T> {
    read_layered_depth(roots, relative, 0)
}

fn read_layered_depth<T: Serialize + DeserializeOwned>(
    roots: &[&Path],
    relative: &Path,
    depth: u32,
) -> anyhow::Result<T> {
    anyhow::ensure!(depth < 8, "{}: `base` chain too long", relative.display());
    let patch_relative = patch_path(relative);
    let mut patches = Vec::new();
    let mut full = None;
    for root in roots {
        let path = root.join(relative);
        if path.is_file() {
            full = Some(path);
            break;
        }
        let patch = root.join(&patch_relative);
        if patch.is_file() {
            patches.push(patch);
        }
    }
    if patches.is_empty() {
        let path = full.unwrap_or_else(|| roots.last().map_or(relative.to_path_buf(), |r| r.join(relative)));
        return Ok(game_data::read_ron(&path)?);
    }
    // Lowest priority first.
    patches.reverse();
    let mut layers = Vec::with_capacity(patches.len());
    for patch in &patches {
        let text = std::fs::read_to_string(patch)
            .map_err(|err| anyhow::anyhow!("reading {}: {err}", patch.display()))?;
        let value: serde_json::Value = ron::Options::default()
            .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
            .from_str(&text)
            .map_err(|err| anyhow::anyhow!("{}: {err}", patch.display()))?;
        layers.push((patch, value));
    }
    let mut value = match full {
        Some(path) => serde_json::to_value(game_data::read_ron::<T>(&path)?)?,
        None => {
            let base = layers[0].1.get("base").and_then(|b| b.as_str()).ok_or_else(|| {
                anyhow::anyhow!(
                    "{}: nothing to patch (no {} and no `base`)",
                    layers[0].0.display(),
                    relative.display()
                )
            })?;
            let base = relative.with_file_name(format!("{base}.ron"));
            serde_json::to_value(read_layered_depth::<T>(roots, &base, depth + 1)?)?
        }
    };
    for (_, mut layer) in layers {
        if let Some(fields) = layer.as_object_mut() {
            fields.remove("base");
        }
        merge(&mut value, layer);
    }
    serde_json::from_value(value).map_err(|err| {
        anyhow::anyhow!(
            "{} with {}: {err} (write enum values in patches as strings: \"Single\")",
            relative.display(),
            patches.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        )
    })
}

/// Fields of `patch` replace those of `value`; maps merge, everything else is replaced.
fn merge(value: &mut serde_json::Value, patch: serde_json::Value) {
    match (value, patch) {
        (serde_json::Value::Object(fields), serde_json::Value::Object(patch)) => {
            for (key, patch) in patch {
                match fields.get_mut(&key) {
                    Some(field) => merge(field, patch),
                    None => {
                        fields.insert(key, patch);
                    }
                }
            }
        }
        (value, patch) => *value = patch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    enum Mode {
        Single,
        Auto,
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Inner {
        a: f32,
        b: f32,
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Gun {
        name: String,
        rpm: f32,
        modes: Vec<Mode>,
        mesh: Option<String>,
        inner: Inner,
    }

    #[test]
    fn patches_and_bases() {
        let dir = std::env::temp_dir().join(format!("bf2_mods_test_{}", std::process::id()));
        let (base, mod_a, mod_b) = (dir.join("imported"), dir.join("a"), dir.join("b"));
        for root in [&base, &mod_a, &mod_b] {
            std::fs::create_dir_all(root.join("weapons")).unwrap();
        }
        std::fs::write(
            base.join("weapons/gun.ron"),
            r#"(name: "gun", rpm: 600.0, modes: [Auto, Single], mesh: Some("gun.glb"), inner: (a: 1.0, b: 2.0))"#,
        )
        .unwrap();
        std::fs::write(mod_b.join("weapons/gun.patch.ron"), "(rpm: 700.0, inner: (b: 5.0))").unwrap();
        std::fs::write(mod_a.join("weapons/gun.patch.ron"), r#"(rpm: 900.0, modes: ["Single"])"#).unwrap();
        std::fs::write(
            mod_a.join("weapons/fast.patch.ron"),
            r#"(base: "gun", name: "fast", mesh: "fast.glb")"#,
        )
        .unwrap();
        let roots = [mod_a.as_path(), mod_b.as_path(), base.as_path()];
        let gun: Gun = read_layered(&roots, Path::new("weapons/gun.ron")).unwrap();
        assert_eq!(gun.rpm, 900.0);
        assert_eq!(gun.modes, vec![Mode::Single]);
        assert_eq!(gun.inner, Inner { a: 1.0, b: 5.0 });
        let fast: Gun = read_layered(&roots, Path::new("weapons/fast.ron")).unwrap();
        assert_eq!(fast.name, "fast");
        assert_eq!(fast.mesh.as_deref(), Some("fast.glb"));
        assert_eq!(fast.rpm, 900.0);
        // A full file in a mod replaces everything below it, patches included.
        std::fs::write(
            mod_a.join("weapons/gun.ron"),
            r#"(name: "gun", rpm: 100.0, modes: [], mesh: None, inner: (a: 0.0, b: 0.0))"#,
        )
        .unwrap();
        let gun: Gun = read_layered(&roots, Path::new("weapons/gun.ron")).unwrap();
        assert_eq!(gun.rpm, 100.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
