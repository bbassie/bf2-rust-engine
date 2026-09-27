//! Which levels need which shared files, so a client downloads the files of the levels a
//! server plays instead of everything the server has (a full BF2 import is about 4 GB, one
//! level with everything it uses about 0.5 GB).
//!
//! The data says what it uses: RON descriptions name other files by path (`"objects/.../x.glb"`,
//! relative to the imported root), by path relative to themselves (glTF texture URIs, level
//! files) or by name (`"template": "sample_crate"` is `templates/sample_crate.ron`, a kit's
//! weapon `"usrif_m4"` is `weapons/usrif_m4.ron`). Every string in a RON file or a glTF's JSON
//! that matches a shared file is taken as a reference.
//!
//! - A level folder (`levels/<name>/`) belongs to its level.
//! - Files the game loads without a level (`sounds.ron`, `effects/`, `radio/`, `menu/`, ...:
//!   everything outside the folders below, plus [`ALWAYS_NEEDED`]) and what they reference
//!   by path are common: every level needs them.
//! - Every other file belongs to the levels that reference it, directly or through other
//!   files. A file no level references is common too, so a reference the scan misses costs
//!   download size, never a missing file.
//!
//! The result is cached next to the hash index (keyed by every path and hash), so a server
//! with unchanged content doesn't scan again.

use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use super::{ALWAYS_NEEDED, MAX_PATH_BYTES, Manifest};

/// Top-level folders whose files are only needed when something references them. Files
/// anywhere else are loaded by the game directly.
const REFERENCED_DIRS: &[&str] = &["levels", "templates", "objects", "vehicles", "weapons", "kits", "soldiers", "common"];

/// Bigger RON or glTF JSON than this isn't scanned (a big level.ron is about 5 MB).
const MAX_SCAN_BYTES: u64 = 64 << 20;

/// A shared path, lowercase, and the local files with it (one per layer that has it).
struct Node {
    path: String,
    files: Vec<PathBuf>,
}

/// Sets [`Manifest::levels`] and every file's levels. `local[i][j]` is the local file of
/// `manifest.layers[i].files[j]`.
pub fn assign_levels(manifest: &mut Manifest, local: &[Vec<PathBuf>], cache: Option<&Path>) {
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut nodes: Vec<Node> = Vec::new();
    for (layer, files) in manifest.layers.iter().zip(local) {
        for (entry, file) in layer.files.iter().zip(files) {
            let path = entry.path.to_ascii_lowercase();
            let id = *index.entry(path.clone()).or_insert_with(|| {
                nodes.push(Node { path, files: Vec::new() });
                nodes.len() - 1
            });
            nodes[id].files.push(file.clone());
        }
    }

    // Levels: folders with a level.ron. Names as the files spell them.
    let mut levels: Vec<String> = Vec::new();
    for layer in &manifest.layers {
        for entry in &layer.files {
            let parts: Vec<&str> = entry.path.split('/').collect();
            if parts.len() == 3
                && parts[0].eq_ignore_ascii_case("levels")
                && parts[2].eq_ignore_ascii_case("level.ron")
                && !levels.iter().any(|l| l.eq_ignore_ascii_case(parts[1]))
            {
                levels.push(parts[1].to_string());
            }
        }
    }
    levels.sort_by_key(|l| l.to_ascii_lowercase());
    manifest.levels = levels.clone();

    let digest = digest(manifest);
    let assignment = match cache.and_then(|c| load_cache(c, &digest)) {
        Some(cached) => cached,
        None => {
            let assignment = compute(&nodes, &index, &levels);
            if let Some(cache) = cache {
                save_cache(cache, &digest, &assignment);
            }
            assignment
        }
    };
    for layer in &mut manifest.layers {
        for entry in &mut layer.files {
            let names = assignment.get(&entry.path.to_ascii_lowercase());
            entry.levels = names
                .into_iter()
                .flatten()
                .filter_map(|name| levels.iter().position(|l| l.eq_ignore_ascii_case(name)).map(|i| i as u32))
                .collect();
            entry.levels.sort_unstable();
            entry.levels.dedup();
        }
    }
}

/// Lowercase path -> the names of the levels needing it (absent: common).
type Assignment = HashMap<String, Vec<String>>;

fn compute(nodes: &[Node], index: &HashMap<String, usize>, levels: &[String]) -> Assignment {
    // Names: `templates/x.ron` and `weapons/x.patch.ron` are both `x`. Not level files: a
    // string "sounds" isn't a reference to every level's sounds.ron.
    let mut names: HashMap<String, Vec<usize>> = HashMap::new();
    for (id, node) in nodes.iter().enumerate() {
        if node.path.starts_with("levels/") {
            continue;
        }
        let file = node.path.rsplit('/').next().unwrap_or_default();
        let name = file.strip_suffix(".patch.ron").or_else(|| file.strip_suffix(".ron"));
        if let Some(name) = name {
            names.entry(name.to_string()).or_default().push(id);
        }
    }

    // References of every file, read on all cores (it's mostly waiting for the disk).
    let edges: Vec<Mutex<Vec<(usize, bool)>>> = (0..nodes.len()).map(|_| Mutex::new(Vec::new())).collect();
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 16);
    std::thread::scope(|scope| {
        for _ in 0..threads.min(nodes.len().max(1)) {
            scope.spawn(|| {
                loop {
                    let id = next.fetch_add(1, Ordering::Relaxed);
                    let Some(node) = nodes.get(id) else {
                        break;
                    };
                    let dir = node.path.rsplit_once('/').map_or("", |(dir, _)| dir);
                    let mut found = Vec::new();
                    for file in &node.files {
                        for string in strings_of(&node.path, file) {
                            resolve(&string, dir, index, &names, &mut found);
                        }
                    }
                    found.sort_unstable();
                    found.dedup();
                    *edges[id].lock().unwrap() = found;
                }
            });
        }
    });
    let edges: Vec<Vec<(usize, bool)>> = edges.into_iter().map(|e| e.into_inner().unwrap()).collect();

    let level_of = |node: &Node| -> Option<usize> {
        let mut parts = node.path.split('/');
        (parts.next() == Some("levels"))
            .then(|| parts.next())
            .flatten()
            .filter(|_| node.path.matches('/').count() >= 2)
            .and_then(|name| levels.iter().position(|l| l.eq_ignore_ascii_case(name)))
    };
    let global_roots: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| {
            let top = node.path.split('/').next().unwrap_or_default();
            !REFERENCED_DIRS.contains(&top) || ALWAYS_NEEDED.contains(&node.path.as_str())
        })
        .map(|(id, _)| id)
        .collect();
    // What the game loads anyway, and what that references by path. Global tables name
    // every weapon or vehicle (AI, effects) without needing them, so names don't count here.
    let common = closure(&global_roots, &edges, false, &HashSet::new());
    let mut reached: Vec<HashSet<usize>> = Vec::with_capacity(levels.len());
    for level in 0..levels.len() {
        let roots: Vec<usize> = (0..nodes.len()).filter(|&id| level_of(&nodes[id]) == Some(level)).collect();
        reached.push(closure(&roots, &edges, true, &common));
    }

    let mut assignment = Assignment::new();
    for (id, node) in nodes.iter().enumerate() {
        let needed_by: Vec<String> = match level_of(node) {
            Some(level) => vec![levels[level].clone()],
            None if common.contains(&id) => continue,
            None => (0..levels.len())
                .filter(|&level| reached[level].contains(&id))
                .map(|level| levels[level].clone())
                .collect(),
        };
        // Unreferenced: common, to be safe.
        if !needed_by.is_empty() {
            assignment.insert(node.path.clone(), needed_by);
        }
    }
    assignment
}

/// Everything reachable from `roots`, not entering `stop`. `names`: follow references by
/// name too, not only by path.
fn closure(roots: &[usize], edges: &[Vec<(usize, bool)>], names: bool, stop: &HashSet<usize>) -> HashSet<usize> {
    let mut seen: HashSet<usize> = roots.iter().copied().collect();
    let mut stack = roots.to_vec();
    while let Some(id) = stack.pop() {
        for &(next, by_name) in &edges[id] {
            if (names || !by_name) && !stop.contains(&next) && seen.insert(next) {
                stack.push(next);
            }
        }
    }
    seen
}

/// The shared files a string may mean: `(node, by name)`.
fn resolve(
    string: &str,
    dir: &str,
    index: &HashMap<String, usize>,
    names: &HashMap<String, Vec<usize>>,
    out: &mut Vec<(usize, bool)>,
) {
    let s = string.trim();
    if s.is_empty() || s.len() > MAX_PATH_BYTES {
        return;
    }
    let s = s.replace('\\', "/").to_ascii_lowercase();
    if s.contains('/') || s.contains('.') {
        let candidates = [normalize(&s), normalize(&format!("{dir}/{s}"))];
        for candidate in candidates.into_iter().flatten() {
            if let Some(&id) = index.get(&candidate) {
                out.push((id, false));
            }
            // A description may be patched by a mod.
            if let Some(stem) = candidate.strip_suffix(".ron")
                && let Some(&id) = index.get(&format!("{stem}.patch.ron"))
            {
                out.push((id, false));
            }
        }
    } else if let Some(ids) = names.get(&s) {
        out.extend(ids.iter().map(|&id| (id, true)));
    }
}

/// `a/./b/../c` -> `a/c`; `None` if it leaves the root.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The strings in a RON file or a glTF's JSON; nothing for other files.
fn strings_of(path: &str, file: &Path) -> Vec<String> {
    let read = |limit: u64| -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        std::fs::File::open(file).ok()?.take(limit).read_to_end(&mut bytes).ok()?;
        Some(bytes)
    };
    if path.ends_with(".ron") {
        let Some(bytes) = read(MAX_SCAN_BYTES) else {
            return Vec::new();
        };
        ron_strings(&String::from_utf8_lossy(&bytes))
    } else if path.ends_with(".glb") {
        glb_json(file).map(|json| json_strings(&json)).unwrap_or_default()
    } else if path.ends_with(".gltf") {
        read(MAX_SCAN_BYTES)
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .map(|json| json_strings(&json))
            .unwrap_or_default()
    } else {
        Vec::new()
    }
}

/// The JSON chunk of a binary glTF.
fn glb_json(file: &Path) -> Option<serde_json::Value> {
    let mut reader = std::fs::File::open(file).ok()?;
    let mut header = [0u8; 20];
    reader.read_exact(&mut header).ok()?;
    if &header[0..4] != b"glTF" || &header[16..20] != b"JSON" {
        return None;
    }
    let length = u32::from_le_bytes(header[12..16].try_into().ok()?) as u64;
    if length > MAX_SCAN_BYTES {
        return None;
    }
    let mut json = Vec::with_capacity(length as usize);
    reader.take(length).read_to_end(&mut json).ok()?;
    serde_json::from_slice(&json).ok()
}

fn json_strings(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        match value {
            serde_json::Value::String(s) => out.push(s.clone()),
            serde_json::Value::Array(items) => stack.extend(items),
            serde_json::Value::Object(fields) => stack.extend(fields.values()),
            _ => {}
        }
    }
    out
}

/// The contents of the `"..."` strings in RON text (escapes kept as written, comments not
/// skipped: a stray reference only adds a file).
fn ron_strings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut s = String::new();
        let mut escaped = false;
        for c in chars.by_ref() {
            match (escaped, c) {
                (false, '\\') => escaped = true,
                (false, '"') => break,
                (_, c) => {
                    s.push(c);
                    escaped = false;
                }
            }
        }
        out.push(s);
    }
    out
}

/// Identifies the shared files: every path and hash.
fn digest(manifest: &Manifest) -> String {
    let mut lines: Vec<String> = manifest
        .files()
        .map(|(layer, f)| format!("{layer}\t{}\t{}", f.path.to_ascii_lowercase(), f.hash))
        .collect();
    lines.sort();
    let mut hasher = super::ContentHasher::new();
    hasher.update(b"deps v1\n");
    for line in lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    hasher.finish()
}

/// Text: the digest, then `path\tlevel,level` per file that isn't common.
fn load_cache(file: &Path, digest: &str) -> Option<Assignment> {
    let reader = std::io::BufReader::new(std::fs::File::open(file).ok()?);
    let mut lines = reader.lines().map_while(Result::ok);
    if lines.next()? != digest {
        return None;
    }
    let mut assignment = Assignment::new();
    for line in lines {
        let (path, levels) = line.split_once('\t')?;
        assignment.insert(path.to_string(), levels.split(',').map(str::to_string).collect());
    }
    Some(assignment)
}

fn save_cache(file: &Path, digest: &str, assignment: &Assignment) {
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let result = (|| -> std::io::Result<()> {
        let temp = file.with_extension(format!("tmp{}", std::process::id()));
        let mut out = std::io::BufWriter::new(std::fs::File::create(&temp)?);
        writeln!(out, "{digest}")?;
        let mut paths: Vec<&String> = assignment.keys().collect();
        paths.sort();
        for path in paths {
            writeln!(out, "{path}\t{}", assignment[path].join(","))?;
        }
        out.flush()?;
        drop(out);
        std::fs::rename(temp, file)
    })();
    if let Err(err) = result {
        bevy::log::warn!("saving {}: {err}", file.display());
    }
}

#[cfg(test)]
mod tests {
    use super::super::{FileEntry, Layer, MANIFEST_VERSION, hash_bytes};
    use super::*;

    #[test]
    fn strings_and_paths() {
        assert_eq!(ron_strings(r#"(a: "x/y.ron", b: ["q\"z", "w"])"#), ["x/y.ron", "q\"z", "w"]);
        assert_eq!(normalize("objects/a/meshes/../../b/./c.dds").as_deref(), Some("objects/b/c.dds"));
        assert_eq!(normalize("../x.dds"), None);
    }

    #[test]
    fn levels_get_what_they_reference() {
        let dir = std::env::temp_dir().join(format!("bf2_content_deps_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let files: &[(&str, &str)] = &[
            ("levels/a/level.ron", r#"(statics: [{"template": "crate"}], kits: ["rifleman"], heightmap: "heightmap.r16")"#),
            ("levels/a/heightmap.r16", "..."),
            ("levels/b/level.ron", r#"(statics: [{"template": "tower"}, {"template": "crate"}])"#),
            ("templates/crate.ron", r#"(mesh: "objects/crate/meshes/crate.glb")"#),
            ("templates/tower.ron", r#"(mesh: "objects/tower/meshes/tower.glb")"#),
            ("kits/rifleman.patch.ron", r#"(base: "assault", weapons: ["carbine"])"#),
            ("weapons/carbine.ron", r#"(mesh_1p: "objects/weapons/carbine/1p.glb")"#),
            ("objects/crate/meshes/crate.glb", "glb"),
            ("objects/tower/meshes/tower.glb", "glb"),
            ("objects/weapons/carbine/1p.glb", "glb"),
            ("objects/unused/thing.glb", "glb"),
            ("sounds.ron", r#"(sounds: {"step": "common/sound/step.wav"}, weapons: ["carbine"])"#),
            ("common/sound/step.wav", "wav"),
        ];
        let mut entries = Vec::new();
        let mut local = Vec::new();
        for (path, text) in files {
            let file = dir.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, text).unwrap();
            entries.push(FileEntry { path: path.to_string(), size: text.len() as u64, hash: hash_bytes(text.as_bytes()), levels: Vec::new() });
            local.push(file);
        }
        let mut manifest = Manifest {
            version: MANIFEST_VERSION,
            layers: vec![Layer { name: "mod".into(), files: entries, ..Default::default() }],
            ..Default::default()
        };
        let cache = dir.join("deps.txt");
        for _ in 0..2 {
            // The second time from the cache.
            assign_levels(&mut manifest, std::slice::from_ref(&local), Some(&cache));
            assert_eq!(manifest.levels, ["a", "b"]);
            let levels_of = |path: &str| manifest.layers[0].files.iter().find(|f| f.path == path).unwrap().levels.clone();
            assert_eq!(levels_of("levels/a/heightmap.r16"), [0]);
            assert_eq!(levels_of("templates/crate.ron"), [0, 1]);
            assert_eq!(levels_of("objects/crate/meshes/crate.glb"), [0, 1]);
            assert_eq!(levels_of("objects/tower/meshes/tower.glb"), [1]);
            // Through the kit (by name), not through sounds.ron (names don't count there).
            assert_eq!(levels_of("objects/weapons/carbine/1p.glb"), [0]);
            assert!(levels_of("common/sound/step.wav").is_empty());
            assert!(levels_of("sounds.ron").is_empty());
            // Unreferenced: common.
            assert!(levels_of("objects/unused/thing.glb").is_empty());
            let needed: Vec<&str> = manifest.needed(&["b".into()]).map(|(_, f)| f.path.as_str()).collect();
            assert!(needed.contains(&"objects/tower/meshes/tower.glb"));
            assert!(!needed.contains(&"levels/a/level.ron"));
            assert!(!needed.contains(&"objects/weapons/carbine/1p.glb"));
        }
        assert!(cache.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
