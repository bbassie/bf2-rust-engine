//! BF2's vehicle HUD overlays (`menu/hud/hudsetup/vehicles/*.con`): the sights, reticles and
//! frames a vehicle weapon shows, chosen by its `weaponHud.guiIndex`. Only static pictures
//! are kept: no text, bars or anything a HUD variable moves or hides.

use std::collections::HashMap;

use game_data::HudPicture;

use crate::meshes::MeshConverter;

const SETUP: &str = "menu/hud/hudsetup/vehicles/";
const TEXTURES: &str = "menu/hud/texture/";

/// The static pictures of every vehicle HUD, by GUI index.
#[derive(Default)]
pub struct VehicleHuds {
    by_index: HashMap<u32, Vec<HudPicture>>,
}

#[derive(Default)]
struct Node {
    parent: String,
    gui_indices: Vec<u32>,
    picture: Option<([f32; 4], String)>,
    color: [f32; 4],
    /// Shown, placed or tinted by a HUD variable.
    dynamic: bool,
}

impl VehicleHuds {
    pub fn load(converter: &MeshConverter) -> Self {
        let mut nodes: HashMap<String, Node> = HashMap::new();
        let mut files: Vec<&str> = converter.vfs.list(SETUP).filter(|p| p.ends_with(".con")).collect();
        files.sort_unstable();
        for file in files {
            let Ok(text) = converter.vfs.read_text(file) else {
                continue;
            };
            parse(&text, &mut nodes);
        }
        let mut huds = Self::default();
        let mut textures: HashMap<String, Option<String>> = HashMap::new();
        for node in nodes.values() {
            let Some((rect, texture)) = &node.picture else {
                continue;
            };
            if node.dynamic {
                continue;
            }
            // The GUI indices of the nearest split node that has any.
            let mut indices = &node.gui_indices;
            let mut parent = &node.parent;
            for _ in 0..16 {
                if !indices.is_empty() {
                    break;
                }
                let Some(up) = nodes.get(parent) else {
                    break;
                };
                indices = &up.gui_indices;
                parent = &up.parent;
            }
            if indices.is_empty() {
                continue;
            }
            let imported = textures
                .entry(texture.clone())
                .or_insert_with(|| import_texture(converter, texture))
                .clone();
            let Some(imported) = imported else {
                continue;
            };
            for index in indices {
                huds.by_index.entry(*index).or_default().push(HudPicture {
                    texture: imported.clone(),
                    rect: *rect,
                    color: node.color,
                });
            }
        }
        for pictures in huds.by_index.values_mut() {
            pictures.sort_by(|a, b| a.texture.cmp(&b.texture));
        }
        huds
    }

    /// The pictures of a weapon's HUD.
    pub fn sight(&self, gui_index: u32) -> Vec<HudPicture> {
        self.by_index.get(&gui_index).cloned().unwrap_or_default()
    }
}

fn parse(text: &str, nodes: &mut HashMap<String, Node>) {
    let mut in_rem = false;
    let mut current: Option<String> = None;
    for line in text.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(command) = words.first().map(|w| w.to_ascii_lowercase()) else {
            continue;
        };
        match command.as_str() {
            "beginrem" => in_rem = true,
            "endrem" => in_rem = false,
            _ if in_rem || command == "rem" => {}
            "hudbuilder.createsplitnode" if words.len() >= 3 => {
                let name = words[2].to_ascii_lowercase();
                nodes.insert(
                    name.clone(),
                    Node {
                        parent: words[1].to_ascii_lowercase(),
                        ..Default::default()
                    },
                );
                current = Some(name);
            }
            "hudbuilder.createpicturenode" if words.len() >= 7 => {
                let name = words[2].to_ascii_lowercase();
                let number = |i: usize| words[i].parse::<f32>().unwrap_or(0.0);
                nodes.insert(
                    name.clone(),
                    Node {
                        parent: words[1].to_ascii_lowercase(),
                        picture: Some(([number(3), number(4), number(5), number(6)], String::new())),
                        color: [1.0; 4],
                        ..Default::default()
                    },
                );
                current = Some(name);
            }
            // Other node kinds: nothing to show, but their settings mustn't land on the last
            // picture.
            c if c.starts_with("hudbuilder.create") => current = None,
            _ => {
                let Some(node) = current.as_ref().and_then(|name| nodes.get_mut(name)) else {
                    continue;
                };
                match command.as_str() {
                    "hudbuilder.setnodelogicshowvariable" if words.len() >= 4 => {
                        if words[2].eq_ignore_ascii_case("guiindex") && node.picture.is_none() {
                            if let Ok(index) = words[3].parse() {
                                node.gui_indices.push(index);
                            }
                        } else if node.picture.is_some() {
                            node.dynamic = true;
                        }
                    }
                    "hudbuilder.setpicturenodetexture" if words.len() >= 2 => {
                        if let Some((_, texture)) = &mut node.picture {
                            *texture = words[1].replace('\\', "/").to_ascii_lowercase();
                        }
                    }
                    "hudbuilder.setnodecolor" if words.len() >= 5 => {
                        for (i, value) in node.color.iter_mut().enumerate() {
                            *value = words[i + 1].parse().unwrap_or(1.0);
                        }
                    }
                    "hudbuilder.setnodeshowvariable"
                    | "hudbuilder.setnodealphavariable"
                    | "hudbuilder.setpicturenoderotatevariable"
                    | "hudbuilder.setnodeposvariable"
                    | "hudbuilder.setnodergbvariables"
                    | "hudbuilder.setpicturenodevariabletexture" => node.dynamic = true,
                    _ => {}
                }
            }
        }
    }
}

/// Copies a HUD texture into the output (`.tga` files become uncompressed `.dds`, which is
/// what the game loads). The setup names some `.tga`s that ship as `.dds` and vice versa.
fn import_texture(converter: &MeshConverter, texture: &str) -> Option<String> {
    let stem = texture.rsplit_once('.').map_or(texture, |(s, _)| s);
    let dds = format!("{TEXTURES}{stem}.dds");
    if converter.vfs.exists(&dds) {
        return converter.file(&dds);
    }
    let tga = format!("{TEXTURES}{stem}.tga");
    let data = converter.vfs.read(&tga).ok()?;
    let converted = tga_to_dds(&data)?;
    let target = converter.out.join(&dds);
    std::fs::create_dir_all(target.parent()?).ok()?;
    std::fs::write(&target, converted).ok()?;
    Some(dds)
}

/// An uncompressed or RLE true-color TGA as an uncompressed 32-bit BGRA DDS.
fn tga_to_dds(data: &[u8]) -> Option<Vec<u8>> {
    let header = data.get(..18)?;
    let (id_len, color_map, kind) = (header[0] as usize, header[1], header[2]);
    let width = u16::from_le_bytes([header[12], header[13]]) as usize;
    let height = u16::from_le_bytes([header[14], header[15]]) as usize;
    let bytes = header[16] as usize / 8;
    let top_down = header[17] & 0x20 != 0;
    if color_map != 0 || !matches!(kind, 2 | 3 | 10 | 11) || !matches!(bytes, 1 | 3 | 4) || width == 0 || height == 0 {
        return None;
    }
    let mut source = data.get(18 + id_len..)?.iter().copied();
    let pixel = |source: &mut dyn Iterator<Item = u8>| -> Option<[u8; 4]> {
        Some(match bytes {
            1 => {
                let v = source.next()?;
                [v, v, v, 255]
            }
            3 => [source.next()?, source.next()?, source.next()?, 255],
            _ => [source.next()?, source.next()?, source.next()?, source.next()?],
        })
    };
    let count = width * height;
    let mut pixels: Vec<[u8; 4]> = Vec::with_capacity(count);
    if kind >= 9 {
        while pixels.len() < count {
            let packet = source.next()?;
            let run = (packet & 0x7f) as usize + 1;
            if packet & 0x80 != 0 {
                let p = pixel(&mut source)?;
                pixels.extend(std::iter::repeat_n(p, run));
            } else {
                for _ in 0..run {
                    pixels.push(pixel(&mut source)?);
                }
            }
        }
        pixels.truncate(count);
    } else {
        for _ in 0..count {
            pixels.push(pixel(&mut source)?);
        }
    }
    let mut out = Vec::with_capacity(128 + count * 4);
    let u32le = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_le_bytes());
    out.extend_from_slice(b"DDS ");
    u32le(&mut out, 124);
    // Caps, height, width, pitch, pixel format.
    u32le(&mut out, 0x1 | 0x2 | 0x4 | 0x8 | 0x1000);
    u32le(&mut out, height as u32);
    u32le(&mut out, width as u32);
    u32le(&mut out, (width * 4) as u32);
    u32le(&mut out, 0);
    u32le(&mut out, 1);
    out.extend_from_slice(&[0; 44]);
    u32le(&mut out, 32);
    u32le(&mut out, 0x1 | 0x40);
    u32le(&mut out, 0);
    u32le(&mut out, 32);
    for mask in [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0xff00_0000] {
        u32le(&mut out, mask);
    }
    u32le(&mut out, 0x1000);
    out.extend_from_slice(&[0; 16]);
    for row in 0..height {
        let source_row = if top_down { row } else { height - 1 - row };
        for p in &pixels[source_row * width..(source_row + 1) * width] {
            out.extend_from_slice(p);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_gui_indices_and_static_pictures() {
        let mut nodes = HashMap::new();
        parse(
            "hudBuilder.createSplitNode VehicleHuds T90Hud\n\
             hudBuilder.setNodeLogicShowVariable EQUAL GuiIndex 7\n\
             hudBuilder.createPictureNode T90Hud Cross 144 294 512 32\n\
             hudBuilder.setPictureNodeTexture Ingame\\Vehicles\\T90\\Cross.dds\n\
             hudBuilder.setNodeColor 0.9 0.4 0.2 1\n\
             hudBuilder.createPictureNode T90Hud Hit 384 284 32 32\n\
             hudBuilder.setNodeShowVariable HitIndicatorIconShow\n\
             beginrem\n\
             hudBuilder.createPictureNode T90Hud Range 1 2 3 4\n\
             endrem\n",
            &mut nodes,
        );
        assert_eq!(nodes["t90hud"].gui_indices, vec![7]);
        let cross = &nodes["cross"];
        assert_eq!(cross.picture.as_ref().unwrap().1, "ingame/vehicles/t90/cross.dds");
        assert!(!cross.dynamic && (cross.color[0] - 0.9).abs() < 1e-6);
        assert!(nodes["hit"].dynamic);
        assert!(!nodes.contains_key("range"));
    }

    #[test]
    fn converts_bottom_up_tga() {
        // 2x1, 32-bit, bottom-left origin.
        let mut tga = vec![0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 1, 0, 32, 8];
        tga.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let dds = tga_to_dds(&tga).unwrap();
        assert_eq!(&dds[..4], b"DDS ");
        assert_eq!(&dds[128..], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }
}
