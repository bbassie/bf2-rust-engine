//! The `imported://` asset source with mods on top: a path is read from the first mod that
//! has the file, else from `imported/` (see `game_shared::mods`). glTF files find their
//! textures the same way, so a mod can replace a texture without touching the mesh.

use std::path::{Path, PathBuf};

use bevy::{
    asset::io::{
        AssetReader, AssetReaderError, AssetSourceBuilder, PathStream, Reader, file::FileAssetReader,
    },
    tasks::ConditionalSendFuture,
};
use game_shared::config::GamePaths;

/// Reads from the mods (highest priority first), then `imported/`.
struct LayeredReader {
    layers: Vec<(PathBuf, FileAssetReader)>,
}

impl LayeredReader {
    /// The reader of the first layer that has `path` (the last layer if none has it, so
    /// errors name the imported folder).
    fn layer(&self, path: &Path) -> &FileAssetReader {
        self.layers
            .iter()
            .find(|(root, _)| root.join(path).exists())
            .or(self.layers.last())
            .map(|(_, reader)| reader)
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

/// The `imported` asset source: the mods of `paths` over the imported folder.
pub fn imported_source(paths: &GamePaths) -> AssetSourceBuilder {
    let roots: Vec<PathBuf> = paths.roots().into_iter().map(Path::to_path_buf).collect();
    AssetSourceBuilder::new(move || {
        Box::new(LayeredReader {
            layers: roots
                .iter()
                .map(|root| (root.clone(), FileAssetReader::new(root)))
                .collect(),
        })
    })
}
