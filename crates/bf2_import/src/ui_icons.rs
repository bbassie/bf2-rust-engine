//! Interface images from BF2's HUD and menus (see `game_data::ui`), converted to `.dds`:
//! the teams' flag icons and the vehicles' map icons.
//!
//! BF2 keeps a folder of flag icons per team name under
//! `Menu/HUD/Texture/Ingame/Flags/Icons` (`Minimap/<team>/miniMap_CP.tga`, the flag on a
//! pole the map shows for a control point; `miniMap_CPBase.tga` for a main base;
//! `Hud/Score/<team>/scoreBoard_Flag.tga`) and menu flags named after the team in
//! `Menu/External/FlashMenu/images/joingame` (`flag_<team>.png`, `flagLarge_<team>.png`;
//! despite the extension these are DDS files). Vehicles name their map icon in
//! `vehicleHud.miniMapIcon` (relative to `Menu/HUD/Texture`; the `.tga` named there is
//! usually a `.dds`), their kind in `vehicleHud.vehicleType` and whether the map shows their
//! turret in `vehicleHud.hasTurretIcon`. `.tga`s are decoded and written as uncompressed
//! BGRA `.dds`.

use std::{collections::BTreeMap, path::Path};

use anyhow::{Result, bail, ensure};
use bf2_formats::{
    con::Interpreter,
    vfs::{Vfs, normalize},
};
use game_data::{TeamIcons, VehicleClass, VehicleIcon};

const FLAGS: &str = "menu/hud/texture/ingame/flags/icons";
const MENU_FLAGS: &str = "menu/external/flashmenu/images/joingame";
const HUD_TEXTURES: &str = "menu/hud/texture";

/// The flag icons of the team called `name` (`MEC`, `US`, `SAS`, ...).
pub fn team_icons(vfs: &Vfs, out: &Path, name: &str) -> TeamIcons {
    let team = name.trim().to_ascii_lowercase();
    if team.is_empty() {
        return TeamIcons::default();
    }
    let icons = TeamIcons {
        flag: image(vfs, out, &format!("{FLAGS}/hud/score/{team}/scoreboard_flag.tga")),
        map: image(vfs, out, &format!("{FLAGS}/minimap/{team}/minimap_cp.tga")),
        base: image(vfs, out, &format!("{FLAGS}/minimap/{team}/minimap_cpbase.tga")),
        menu: image(vfs, out, &format!("{MENU_FLAGS}/flag_{team}.png")),
        large: image(vfs, out, &format!("{MENU_FLAGS}/flaglarge_{team}.png")),
    };
    if icons.map.is_none() || icons.flag.is_none() {
        log::warn!("no flag icons for team {name}");
    }
    icons
}

/// A neutral control point's icon.
pub fn neutral_icons(vfs: &Vfs, out: &Path) -> TeamIcons {
    TeamIcons {
        map: image(vfs, out, &format!("{FLAGS}/minimap/neutral/minimap_cp.tga")),
        ..Default::default()
    }
}

/// How the vehicles show on the maps, by template.
pub fn vehicle_icons(interp: &mut Interpreter, vfs: &Vfs, out: &Path, names: &[String]) -> BTreeMap<String, VehicleIcon> {
    let mut icons = BTreeMap::new();
    for name in names {
        let name = name.to_ascii_lowercase();
        interp.ensure_template(&name);
        let Some(template) = interp.world.template(&name) else {
            continue;
        };
        let icon = template
            .get_str("vehiclehud.minimapicon")
            .and_then(|path| image(vfs, out, &format!("{HUD_TEXTURES}/{}", path.trim_matches('"'))));
        let class = match template.get_f32("vehiclehud.vehicletype").map(|t| t as i32) {
            Some(0) => VehicleClass::Tank,
            Some(1) => VehicleClass::Apc,
            Some(2) => VehicleClass::Helicopter,
            Some(4) => VehicleClass::Jet,
            Some(5) => VehicleClass::AntiAir,
            Some(6) => VehicleClass::Boat,
            Some(_) => VehicleClass::Jeep,
            None => VehicleClass::Stationary,
        };
        let turret = template.get_f32("vehiclehud.hasturreticon").is_some_and(|t| t != 0.0);
        if icon.is_none() {
            log::debug!("{name}: no map icon");
        }
        icons.insert(name, VehicleIcon { icon, class, turret });
    }
    icons
}

/// A weapon's icon (`weaponHud.selectIcon`, relative to BF2's `Menu/HUD/Texture`) as a white
/// silhouette: BF2 draws these dark shapes tinted by its HUD; ours keeps their alpha and
/// tints them itself. Written as `<name>_white.dds`.
pub fn hud_image(vfs: &Vfs, out: &Path, path: &str) -> Option<String> {
    let path = normalize(&format!("{HUD_TEXTURES}/{path}"));
    let stem = path.rsplit_once('.').map_or(path.as_str(), |(stem, _)| stem);
    let source = format!("{stem}.tga");
    if !vfs.exists(&source) {
        // Not a TGA: as it is.
        return image(vfs, out, &path);
    }
    let target = format!("{stem}_white.dds");
    let file = out.join(&target);
    if !file.exists() {
        let (width, height, mut bgra) = decode_tga(&vfs.read(&source).ok()?)
            .map_err(|err| log::warn!("{source}: {err:#}"))
            .ok()?;
        for pixel in bgra.chunks_exact_mut(4) {
            pixel[..3].fill(255);
        }
        std::fs::create_dir_all(file.parent()?).ok()?;
        std::fs::write(&file, bgra8_dds(width, height, &bgra)).ok()?;
    }
    Some(target)
}

/// Copies or converts an image to `<out>/<path>.dds`; returns that path. Looks for the file
/// as named, then as `.dds` and `.tga`.
fn image(vfs: &Vfs, out: &Path, path: &str) -> Option<String> {
    let path = normalize(path);
    let stem = path.rsplit_once('.').map_or(path.as_str(), |(stem, _)| stem);
    let source = [path.clone(), format!("{stem}.dds"), format!("{stem}.tga")]
        .into_iter()
        .find(|p| vfs.exists(p))?;
    let target = format!("{stem}.dds");
    let file = out.join(&target);
    if !file.exists() {
        let data = vfs.read(&source).ok()?;
        let dds = if data.starts_with(b"DDS ") {
            data
        } else {
            match decode_tga(&data) {
                Ok((width, height, bgra)) => bgra8_dds(width, height, &bgra),
                Err(err) => {
                    log::warn!("{source}: {err:#}");
                    return None;
                }
            }
        };
        std::fs::create_dir_all(file.parent()?).ok()?;
        std::fs::write(&file, dds).ok()?;
    }
    Some(target)
}

/// Decodes an uncompressed or RLE true-colour TGA (24 or 32 bit) into BGRA rows, top first.
fn decode_tga(data: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    ensure!(data.len() >= 18, "not a TGA file");
    let id_length = data[0] as usize;
    let color_map = data[1];
    let kind = data[2];
    let map_length = u16::from_le_bytes([data[5], data[6]]) as usize;
    let map_bits = data[7] as usize;
    let width = u16::from_le_bytes([data[12], data[13]]) as u32;
    let height = u16::from_le_bytes([data[14], data[15]]) as u32;
    let bits = data[16];
    let top_first = data[17] & 0x20 != 0;
    if !matches!(kind, 2 | 10) || !matches!(bits, 24 | 32) {
        bail!("unsupported TGA (type {kind}, {bits} bit)");
    }
    let bytes = bits as usize / 8;
    let mut at = 18 + id_length + if color_map != 0 { map_length * map_bits.div_ceil(8) } else { 0 };
    let pixels = (width * height) as usize;
    let mut bgra = Vec::with_capacity(pixels * 4);
    let push = |px: &[u8], out: &mut Vec<u8>| {
        out.extend_from_slice(&[px[0], px[1], px[2], if bytes == 4 { px[3] } else { 255 }]);
    };
    if kind == 2 {
        ensure!(data.len() >= at + pixels * bytes, "TGA too short");
        for px in data[at..at + pixels * bytes].chunks_exact(bytes) {
            push(px, &mut bgra);
        }
    } else {
        while bgra.len() < pixels * 4 {
            let header = *data.get(at).ok_or_else(|| anyhow::anyhow!("TGA too short"))?;
            at += 1;
            let count = (header & 0x7f) as usize + 1;
            if header & 0x80 != 0 {
                let px = data.get(at..at + bytes).ok_or_else(|| anyhow::anyhow!("TGA too short"))?;
                for _ in 0..count {
                    push(px, &mut bgra);
                }
                at += bytes;
            } else {
                let run = data.get(at..at + count * bytes).ok_or_else(|| anyhow::anyhow!("TGA too short"))?;
                for px in run.chunks_exact(bytes) {
                    push(px, &mut bgra);
                }
                at += count * bytes;
            }
        }
        bgra.truncate(pixels * 4);
    }
    if !top_first {
        let row = width as usize * 4;
        let flipped: Vec<u8> = bgra.chunks_exact(row).rev().flatten().copied().collect();
        bgra = flipped;
    }
    Ok((width, height, bgra))
}

/// An uncompressed A8R8G8B8 DDS (one mip level).
fn bgra8_dds(width: u32, height: u32, bgra: &[u8]) -> Vec<u8> {
    let mut dds = vec![0u8; 128];
    let mut put = |offset: usize, value: u32| dds[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    put(0, u32::from_le_bytes(*b"DDS "));
    put(4, 124);
    // Caps, height, width, pixel format, pitch.
    put(8, 0x1 | 0x2 | 0x4 | 0x1000 | 0x8);
    put(12, height);
    put(16, width);
    put(20, width * 4);
    put(28, 1);
    put(76, 32);
    // DDPF_RGB | DDPF_ALPHAPIXELS, 32 bit, R G B A masks.
    put(80, 0x40 | 0x1);
    put(88, 32);
    put(92, 0x00ff_0000);
    put(96, 0x0000_ff00);
    put(100, 0x0000_00ff);
    put(104, 0xff00_0000);
    put(108, 0x1000);
    dds.extend_from_slice(bgra);
    dds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_bottom_up_tga() {
        // 2x2, 32 bit, bottom-left origin: rows bottom first.
        let mut tga = vec![0u8; 18];
        tga[2] = 2;
        tga[12] = 2;
        tga[14] = 2;
        tga[16] = 32;
        tga[17] = 8;
        tga.extend_from_slice(&[1, 1, 1, 255, 2, 2, 2, 255]); // bottom row
        tga.extend_from_slice(&[3, 3, 3, 255, 4, 4, 4, 255]); // top row
        let (w, h, bgra) = decode_tga(&tga).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(&bgra[..8], &[3, 3, 3, 255, 4, 4, 4, 255]);
        let dds = bgra8_dds(w, h, &bgra);
        assert_eq!(dds.len(), 128 + 16);
        assert_eq!(&dds[..4], b"DDS ");
    }

    #[test]
    fn decodes_rle_tga() {
        let mut tga = vec![0u8; 18];
        tga[2] = 10;
        tga[12] = 3;
        tga[14] = 1;
        tga[16] = 24;
        tga[17] = 0x20;
        tga.extend_from_slice(&[0x82, 9, 8, 7]); // 3 repeated pixels
        let (_, _, bgra) = decode_tga(&tga).unwrap();
        assert_eq!(bgra, [9, 8, 7, 255].repeat(3));
    }
}
