//! A small binary glTF (`.glb`) writer: meshes, materials with external textures, one scene.

use std::path::Path;

use serde_json::{Value, json};

/// Vertex data for one primitive, already in engine coordinates.
#[derive(Default)]
pub struct Primitive {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub tangents: Vec<[f32; 4]>,
    /// Up to two UV sets (`TEXCOORD_0`, `TEXCOORD_1`).
    pub uvs: Vec<Vec<[f32; 2]>>,
    /// `COLOR_0` (linear RGBA).
    pub colors: Vec<[f32; 4]>,
    pub joints: Vec<[u16; 4]>,
    pub weights: Vec<[f32; 4]>,
    pub indices: Vec<u32>,
    pub material: Option<usize>,
}

pub struct Mesh {
    pub name: String,
    pub primitives: Vec<Primitive>,
}

pub struct Material {
    pub name: String,
    /// Image index for the base color.
    pub base_color: Option<usize>,
    pub base_color_uv: u32,
    pub normal: Option<usize>,
    pub alpha: AlphaMode,
    pub double_sided: bool,
    /// Free-form data kept in `extras` (BF2 technique, all texture maps, ...).
    pub extras: Value,
}

#[derive(Clone, Copy, PartialEq)]
pub enum AlphaMode {
    Opaque,
    Mask(f32),
    Blend,
}

pub struct Node {
    pub name: String,
    pub mesh: Option<usize>,
    pub skin: Option<usize>,
    pub translation: [f32; 3],
    pub rotation: [f32; 4],
    pub children: Vec<usize>,
}

impl Default for Node {
    fn default() -> Self {
        Self {
            name: String::new(),
            mesh: None,
            skin: None,
            translation: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            children: Vec::new(),
        }
    }
}

/// Joints (node indices) and their inverse bind matrices (column-major).
pub struct Skin {
    pub joints: Vec<usize>,
    pub inverse_bind_matrices: Vec<[f32; 16]>,
}

pub struct Animation {
    pub name: String,
    pub channels: Vec<Channel>,
}

pub struct Channel {
    pub node: usize,
    /// Seconds.
    pub times: Vec<f32>,
    pub values: ChannelValues,
}

pub enum ChannelValues {
    Translation(Vec<[f32; 3]>),
    Rotation(Vec<[f32; 4]>),
}

#[derive(Default)]
pub struct Document {
    pub meshes: Vec<Mesh>,
    pub materials: Vec<Material>,
    /// Image URIs relative to the `.glb`.
    pub images: Vec<String>,
    pub nodes: Vec<Node>,
    pub skins: Vec<Skin>,
    pub animations: Vec<Animation>,
    /// Root nodes of the default scene.
    pub scene: Vec<usize>,
    pub extras: Value,
}

struct Builder {
    bin: Vec<u8>,
    views: Vec<Value>,
    accessors: Vec<Value>,
}

impl Builder {
    fn view(&mut self, bytes: &[u8], target: Option<u32>) -> usize {
        while !self.bin.len().is_multiple_of(4) {
            self.bin.push(0);
        }
        let mut view = json!({
            "buffer": 0,
            "byteOffset": self.bin.len(),
            "byteLength": bytes.len(),
        });
        if let Some(target) = target {
            view["target"] = json!(target);
        }
        self.bin.extend_from_slice(bytes);
        self.views.push(view);
        self.views.len() - 1
    }

    fn floats<const N: usize>(&mut self, data: &[[f32; N]], with_bounds: bool) -> usize {
        self.floats_with_target(data, with_bounds, Some(34962))
    }

    /// Float accessor; `target` is `None` for non-vertex data (animation, matrices).
    fn floats_with_target<const N: usize>(
        &mut self,
        data: &[[f32; N]],
        with_bounds: bool,
        target: Option<u32>,
    ) -> usize {
        let bytes: Vec<u8> = data.iter().flatten().flat_map(|f| f.to_le_bytes()).collect();
        let view = self.view(&bytes, target);
        let kind = match N {
            1 => "SCALAR",
            2 => "VEC2",
            3 => "VEC3",
            4 => "VEC4",
            16 => "MAT4",
            _ => "SCALAR",
        };
        let mut accessor = json!({
            "bufferView": view,
            "componentType": 5126,
            "count": data.len(),
            "type": kind,
        });
        if with_bounds {
            let mut min = [f32::MAX; N];
            let mut max = [f32::MIN; N];
            for v in data {
                for i in 0..N {
                    min[i] = min[i].min(v[i]);
                    max[i] = max[i].max(v[i]);
                }
            }
            accessor["min"] = json!(min.to_vec());
            accessor["max"] = json!(max.to_vec());
        }
        self.accessors.push(accessor);
        self.accessors.len() - 1
    }

    fn joints(&mut self, data: &[[u16; 4]]) -> usize {
        let bytes: Vec<u8> = data.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
        let view = self.view(&bytes, Some(34962));
        self.accessors.push(json!({
            "bufferView": view,
            "componentType": 5123,
            "count": data.len(),
            "type": "VEC4",
        }));
        self.accessors.len() - 1
    }

    fn indices(&mut self, data: &[u32]) -> usize {
        let bytes: Vec<u8> = data.iter().flat_map(|v| v.to_le_bytes()).collect();
        let view = self.view(&bytes, Some(34963));
        self.accessors.push(json!({
            "bufferView": view,
            "componentType": 5125,
            "count": data.len(),
            "type": "SCALAR",
        }));
        self.accessors.len() - 1
    }
}

impl Document {
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let mut b = Builder {
            bin: Vec::new(),
            views: Vec::new(),
            accessors: Vec::new(),
        };

        let meshes: Vec<Value> = self
            .meshes
            .iter()
            .map(|mesh| {
                let primitives: Vec<Value> = mesh
                    .primitives
                    .iter()
                    .filter(|p| !p.indices.is_empty() && !p.positions.is_empty())
                    .map(|p| {
                        let mut attributes = serde_json::Map::new();
                        attributes.insert("POSITION".into(), json!(b.floats(&p.positions, true)));
                        if p.normals.len() == p.positions.len() {
                            attributes.insert("NORMAL".into(), json!(b.floats(&p.normals, false)));
                        }
                        if p.tangents.len() == p.positions.len() {
                            attributes.insert("TANGENT".into(), json!(b.floats(&p.tangents, false)));
                        }
                        for (i, uv) in p.uvs.iter().enumerate() {
                            if uv.len() == p.positions.len() {
                                attributes.insert(format!("TEXCOORD_{i}"), json!(b.floats(uv, false)));
                            }
                        }
                        if p.colors.len() == p.positions.len() {
                            attributes.insert("COLOR_0".into(), json!(b.floats(&p.colors, false)));
                        }
                        if p.joints.len() == p.positions.len() && p.weights.len() == p.positions.len() {
                            attributes.insert("JOINTS_0".into(), json!(b.joints(&p.joints)));
                            attributes.insert("WEIGHTS_0".into(), json!(b.floats(&p.weights, false)));
                        }
                        let mut primitive = json!({
                            "attributes": attributes,
                            "indices": b.indices(&p.indices),
                            "mode": 4,
                        });
                        if let Some(material) = p.material {
                            primitive["material"] = json!(material);
                        }
                        primitive
                    })
                    .collect();
                json!({ "name": mesh.name, "primitives": primitives })
            })
            .collect();

        let materials: Vec<Value> = self
            .materials
            .iter()
            .map(|m| {
                let mut pbr = json!({ "metallicFactor": 0.0, "roughnessFactor": 0.9 });
                if let Some(image) = m.base_color {
                    pbr["baseColorTexture"] = json!({ "index": image, "texCoord": m.base_color_uv });
                }
                let mut material = json!({
                    "name": m.name,
                    "pbrMetallicRoughness": pbr,
                    "doubleSided": m.double_sided,
                });
                if let Some(image) = m.normal {
                    material["normalTexture"] = json!({ "index": image });
                }
                match m.alpha {
                    AlphaMode::Opaque => {}
                    AlphaMode::Mask(cutoff) => {
                        material["alphaMode"] = json!("MASK");
                        material["alphaCutoff"] = json!(cutoff);
                    }
                    AlphaMode::Blend => material["alphaMode"] = json!("BLEND"),
                }
                if !m.extras.is_null() {
                    material["extras"] = m.extras.clone();
                }
                material
            })
            .collect();

        // One texture per image, all sharing a repeating sampler.
        let images: Vec<Value> = self.images.iter().map(|uri| json!({ "uri": uri })).collect();
        let textures: Vec<Value> = (0..self.images.len())
            .map(|i| json!({ "source": i, "sampler": 0 }))
            .collect();

        let nodes: Vec<Value> = self
            .nodes
            .iter()
            .map(|n| {
                let mut node = json!({ "name": n.name });
                if let Some(mesh) = n.mesh {
                    node["mesh"] = json!(mesh);
                }
                if let Some(skin) = n.skin {
                    node["skin"] = json!(skin);
                }
                if n.translation != [0.0; 3] {
                    node["translation"] = json!(n.translation);
                }
                if n.rotation != [0.0, 0.0, 0.0, 1.0] {
                    node["rotation"] = json!(n.rotation);
                }
                if !n.children.is_empty() {
                    node["children"] = json!(n.children);
                }
                node
            })
            .collect();

        let skins: Vec<Value> = self
            .skins
            .iter()
            .map(|skin| {
                json!({
                    "joints": skin.joints,
                    "inverseBindMatrices": b.floats_with_target(&skin.inverse_bind_matrices, false, None),
                })
            })
            .collect();

        let animations: Vec<Value> = self
            .animations
            .iter()
            .map(|animation| {
                let mut samplers = Vec::new();
                let mut channels = Vec::new();
                for channel in &animation.channels {
                    let times: Vec<[f32; 1]> = channel.times.iter().map(|&t| [t]).collect();
                    let input = b.floats_with_target(&times, true, None);
                    let (output, path) = match &channel.values {
                        ChannelValues::Translation(v) => (b.floats_with_target(v, false, None), "translation"),
                        ChannelValues::Rotation(v) => (b.floats_with_target(v, false, None), "rotation"),
                    };
                    samplers.push(json!({ "input": input, "output": output, "interpolation": "LINEAR" }));
                    channels.push(json!({
                        "sampler": samplers.len() - 1,
                        "target": { "node": channel.node, "path": path },
                    }));
                }
                json!({ "name": animation.name, "samplers": samplers, "channels": channels })
            })
            .collect();

        while !b.bin.len().is_multiple_of(4) {
            b.bin.push(0);
        }
        let mut root = json!({
            "asset": { "version": "2.0", "generator": "bf2_import" },
            "scene": 0,
            "scenes": [{ "nodes": self.scene }],
            "nodes": nodes,
            "meshes": meshes,
            "buffers": [{ "byteLength": b.bin.len() }],
            "bufferViews": b.views,
            "accessors": b.accessors,
        });
        if !materials.is_empty() {
            root["materials"] = json!(materials);
        }
        if !images.is_empty() {
            root["images"] = json!(images);
            root["textures"] = json!(textures);
            root["samplers"] = json!([{ "magFilter": 9729, "minFilter": 9987, "wrapS": 10497, "wrapT": 10497 }]);
        }
        if !skins.is_empty() {
            root["skins"] = json!(skins);
        }
        if !animations.is_empty() {
            root["animations"] = json!(animations);
        }
        if !self.extras.is_null() {
            root["extras"] = self.extras.clone();
        }

        let mut json_bytes = serde_json::to_vec(&root).map_err(std::io::Error::other)?;
        while !json_bytes.len().is_multiple_of(4) {
            json_bytes.push(b' ');
        }
        let total = 12 + 8 + json_bytes.len() + 8 + b.bin.len();
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&0x4654_6C67u32.to_le_bytes()); // "glTF"
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&0x4E4F_534Au32.to_le_bytes()); // "JSON"
        out.extend_from_slice(&json_bytes);
        out.extend_from_slice(&(b.bin.len() as u32).to_le_bytes());
        out.extend_from_slice(&0x004E_4942u32.to_le_bytes()); // "BIN\0"
        out.extend_from_slice(&b.bin);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, out)
    }
}

/// `to` relative to the folder containing `from` (both relative to the same root, `/`-separated).
pub fn relative_uri(from_file: &str, to_file: &str) -> String {
    let from_dir: Vec<&str> = from_file.split('/').collect();
    let from_dir = &from_dir[..from_dir.len().saturating_sub(1)];
    let to: Vec<&str> = to_file.split('/').collect();
    let common = from_dir
        .iter()
        .zip(&to)
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<&str> = vec![".."; from_dir.len() - common];
    parts.extend_from_slice(&to[common..]);
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_uris() {
        assert_eq!(
            relative_uri("objects/a/meshes/x.glb", "objects/a/textures/x_c.dds"),
            "../textures/x_c.dds"
        );
        assert_eq!(relative_uri("objects/a/x.glb", "common/t.dds"), "../../common/t.dds");
    }
}
