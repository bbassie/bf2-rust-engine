//! The `imported://` asset source with mods on top: a path is read from the first mod that
//! has the file, else from `imported/` (see `game_shared::mods`). glTF files find their
//! textures the same way, so a mod can replace a texture without touching the mesh.
//!
//! The layers can change while the game runs: joining a server that shares its content
//! mounts the server's layers for the session (see `content`), leaving goes back.

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, RwLock},
};

use bevy::{
    asset::io::{
        AssetReader, AssetReaderError, AssetSourceBuilder, PathStream, Reader, file::FileAssetReader,
    },
    tasks::ConditionalSendFuture,
};
use game_shared::config::GamePaths;

/// A folder and its reader. Kept for the whole run (one per folder ever used), so readers
/// can hand out readers borrowing it while the layers change.
struct Layer {
    root: PathBuf,
    reader: FileAssetReader,
}

/// Every layer ever used, and the current ones (highest priority first).
static ALL_LAYERS: Mutex<Vec<&'static Layer>> = Mutex::new(Vec::new());
static LAYERS: RwLock<Vec<&'static Layer>> = RwLock::new(Vec::new());

/// Reads `imported://` paths from the layers of `paths` from now on.
pub fn set_layers(paths: &GamePaths) {
    let mut all = ALL_LAYERS.lock().unwrap();
    let layers = paths
        .roots()
        .into_iter()
        .map(|root| match all.iter().find(|l| l.root == root) {
            Some(layer) => *layer,
            None => {
                let layer: &'static Layer = Box::leak(Box::new(Layer {
                    root: root.to_path_buf(),
                    reader: FileAssetReader::new(root),
                }));
                all.push(layer);
                layer
            }
        })
        .collect();
    *LAYERS.write().unwrap() = layers;
}

/// Reads from the mods (highest priority first), then `imported/`.
struct LayeredReader;

impl LayeredReader {
    /// The reader of the first layer that has `path` (the last layer if none has it, so
    /// errors name the imported folder).
    fn layer(&self, path: &Path) -> &'static FileAssetReader {
        let layers = LAYERS.read().unwrap();
        layers
            .iter()
            .find(|layer| layer.root.join(path).exists())
            .or(layers.last())
            .map(|layer| &layer.reader)
            .expect("at least the imported folder")
    }
}

impl AssetReader for LayeredReader {
    async fn read<'a>(&'a self, path: &'a Path) -> Result<impl Reader + 'a, AssetReaderError> {
        self.layer(path).read(path).await
    }

    async fn read_meta<'a>(&'a self, path: &'a Path) -> Result<impl Reader + 'a, AssetReaderError> {
        // Meta files belong to the layer the asset comes from.
        self.layer(path).read_meta(path).await
    }

    fn read_directory<'a>(
        &'a self,
        path: &'a Path,
    ) -> impl ConditionalSendFuture<Output = Result<Box<PathStream>, AssetReaderError>> {
        self.layer(path).read_directory(path)
    }

    fn is_directory<'a>(
        &'a self,
        path: &'a Path,
    ) -> impl ConditionalSendFuture<Output = Result<bool, AssetReaderError>> {
        self.layer(path).is_directory(path)
    }
}

/// The `imported` asset source: the mods of `paths` over the imported folder (changed later
/// with [`set_layers`]).
pub fn imported_source(paths: &GamePaths) -> AssetSourceBuilder {
    set_layers(paths);
    AssetSourceBuilder::new(|| Box::new(LayeredReader))
}
