//! Skeletons (`.ske`, version 2) and animations (`.baf`, versions 3 and 4).
//!
//! Both store the *conjugate* of each bone's local rotation; [`Skeleton`] and [`Animation`]
//! return the true rotation. Transforms are parent-relative, in BF2's left-handed space.
//!
//! Version 4 (used by almost every BF2 animation) names each track by its index into a
//! companion `.ske`. Version 3 (rare; some AIX 2 weapon animations, e.g.
//! `3p_aix_portableminigun_*.baf`, an older Refractor 2 export) instead spells out each
//! track's node name inline (null-terminated, `u16`-length-prefixed) and has no companion
//! `.ske` of its own. Its names mix skeleton bone names (`Camerabone`, `L_collar`, `mesh1`..)
//! with the weapon's own rigid-part hierarchy (`root_BundledMesh_*`, `geom0`, `lod0`,
//! `..PlayerControlObject`, `..RotationalBundle`, `..Anchor`): only the former map onto a
//! character skeleton, so [`Animation::parse`] resolves names against the skeleton passed in
//! and drops tracks that don't match one of its bones. Everything after the per-track header
//! (frame count, position precision, per-bone keyframe streams) is byte-identical between the
//! two versions.

use crate::reader::{ReadError, Reader};

#[derive(Clone, Debug)]
pub struct Bone {
    pub name: String,
    /// Parent index, `None` for roots. Parents always come before their children.
    pub parent: Option<usize>,
    /// Local rotation (x, y, z, w), already un-conjugated.
    pub rotation: [f32; 4],
    pub translation: [f32; 3],
}

#[derive(Clone, Debug)]
pub struct Skeleton {
    pub bones: Vec<Bone>,
}

impl Skeleton {
    pub fn parse(data: &[u8]) -> Result<Self, ReadError> {
        let mut r = Reader::new(data);
        let version = r.u32()?;
        if version != 2 {
            return Err(ReadError::Invalid(format!("unsupported skeleton version {version}")));
        }
        let count = r.count(30)?;
        let mut bones = Vec::with_capacity(count);
        for index in 0..count {
            let name_len = r.u16()? as usize;
            let name: String = r
                .bytes(name_len)?
                .iter()
                .take_while(|&&b| b != 0)
                .map(|&b| b as char)
                .collect();
            let parent = r.i16()?;
            let q = [r.f32()?, r.f32()?, r.f32()?, r.f32()?];
            let t = r.vec3()?;
            let parent = (parent >= 0 && (parent as usize) < index).then_some(parent as usize);
            // Weapon part bones (`mesh1..16`) hold uninitialized garbage; code drives them.
            let garbage = q.iter().chain(&t).any(|v| !v.is_finite() || v.abs() > 1e4);
            let (rotation, translation) = if garbage {
                ([0.0, 0.0, 0.0, 1.0], [0.0; 3])
            } else {
                ([-q[0], -q[1], -q[2], q[3]], t)
            };
            bones.push(Bone {
                name,
                parent,
                rotation,
                translation,
            });
        }
        Ok(Self { bones })
    }

    pub fn find(&self, name: &str) -> Option<usize> {
        self.bones.iter().position(|b| b.name.eq_ignore_ascii_case(name))
    }
}

/// Keyframes of one bone.
#[derive(Clone, Debug)]
pub struct BoneTrack {
    /// Index into the skeleton's bones.
    pub bone: usize,
    /// One rotation per frame (x, y, z, w), un-conjugated.
    pub rotations: Vec<[f32; 4]>,
    /// One translation per frame.
    pub translations: Vec<[f32; 3]>,
}

#[derive(Clone, Debug)]
pub struct Animation {
    pub frame_count: usize,
    pub tracks: Vec<BoneTrack>,
}

impl Animation {
    /// BF2 plays animations at a fixed 24 frames per second.
    pub const FPS: f32 = 24.0;

    pub fn duration(&self) -> f32 {
        self.frame_count.saturating_sub(1) as f32 / Self::FPS
    }

    /// `skeleton` resolves version 3's inline bone names; version 4 (the common case) never
    /// needs it, so `None` is fine unless a version 3 file turns up.
    pub fn parse(data: &[u8], skeleton: Option<&Skeleton>) -> Result<Self, ReadError> {
        let mut r = Reader::new(data);
        let version = r.u32()?;
        if version != 3 && version != 4 {
            return Err(ReadError::Invalid(format!("unsupported animation version {version}")));
        }
        let bone_count = r.u16()? as usize;
        // `None` for a version 3 track whose name isn't one of the skeleton's bones (the
        // weapon's own rigid-part nodes): the stream is still parsed, just not kept.
        let bone_ids: Vec<Option<usize>> = if version == 4 {
            (0..bone_count).map(|_| r.u16().map(|v| Some(v as usize))).collect::<Result<_, _>>()?
        } else {
            let skeleton = skeleton
                .ok_or_else(|| ReadError::Invalid("version 3 animation needs a skeleton to resolve bone names".into()))?;
            (0..bone_count)
                .map(|_| {
                    let name_len = r.u16()? as usize;
                    let name: String = r
                        .bytes(name_len)?
                        .iter()
                        .take_while(|&&b| b != 0)
                        .map(|&b| b as char)
                        .collect();
                    Ok(skeleton.find(&name))
                })
                .collect::<Result<_, ReadError>>()?
        };
        let frame_count = r.u32()? as usize;
        if frame_count == 0 || frame_count > 100_000 {
            return Err(ReadError::Invalid(format!("implausible frame count {frame_count}")));
        }
        let precision = r.u8()? as i32;
        let position_scale = 2f32.powi(15 - precision) / 32767.0;

        let mut tracks = Vec::with_capacity(bone_count);
        for &bone in &bone_ids {
            let _data_size = r.u16()?;
            let mut streams: [Vec<i16>; 7] = Default::default();
            for stream in &mut streams {
                let mut words_left = r.u16()? as i32;
                while words_left > 0 {
                    let head = r.u8()?;
                    let block_words = r.u8()? as i32;
                    let frames = (head & 0x7F) as usize;
                    if head & 0x80 != 0 {
                        let value = r.i16()?;
                        stream.extend(std::iter::repeat_n(value, frames));
                    } else {
                        for _ in 0..frames {
                            stream.push(r.i16()?);
                        }
                    }
                    words_left -= block_words;
                }
                stream.resize(frame_count, stream.last().copied().unwrap_or(0));
            }
            // Still consumed above regardless of `bone`: version 3 tracks the file doesn't
            // map onto the skeleton (the weapon's own rigid-part nodes) are skipped, but the
            // stream bytes must be read either way to stay aligned for the next bone.
            let Some(bone) = bone else { continue };
            let rot = |s: usize, f: usize| streams[s][f] as f32 / 32767.0;
            let pos = |s: usize, f: usize| streams[s][f] as f32 * position_scale;
            tracks.push(BoneTrack {
                bone,
                rotations: (0..frame_count)
                    .map(|f| [-rot(0, f), -rot(1, f), -rot(2, f), rot(3, f)])
                    .collect(),
                translations: (0..frame_count).map(|f| [pos(4, f), pos(5, f), pos(6, f)]).collect(),
            });
        }
        Ok(Self {
            frame_count,
            tracks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One frame, one 16-bit keyframe per channel, as an all-RLE block (BF2's usual encoding
    /// for a still pose): `stream_size=2` words, one `head=0x81` (RLE, 1 frame) + `value`.
    fn bone_stream(value: i16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(2u16.to_le_bytes()); // stream_size (words)
        out.push(0x81); // RLE, 1 frame
        out.push(2); // block size (words)
        out.extend(value.to_le_bytes());
        out
    }

    /// A minimal `.baf` with one frame and 7 identical-looking channels per bone (qx..qw,
    /// px..pz), named (version 3) or indexed (version 4).
    fn bone_data() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(14u16.to_le_bytes()); // data_size (unused by the parser)
        for _ in 0..7 {
            out.extend(bone_stream(0));
        }
        out
    }

    #[test]
    fn parses_version_4_by_bone_index() {
        let mut data = Vec::new();
        data.extend(4u32.to_le_bytes());
        data.extend(2u16.to_le_bytes()); // bone_count
        data.extend(5u16.to_le_bytes()); // bone id 0 -> skeleton index 5
        data.extend(1u16.to_le_bytes()); // bone id 1 -> skeleton index 1
        data.extend(1u32.to_le_bytes()); // frame_count
        data.push(0); // precision
        data.extend(bone_data());
        data.extend(bone_data());

        let anim = Animation::parse(&data, None).unwrap();
        assert_eq!(anim.frame_count, 1);
        assert_eq!(anim.tracks.iter().map(|t| t.bone).collect::<Vec<_>>(), vec![5, 1]);
    }

    /// Version 3 (an older export some AIX 2 weapon animations use, e.g.
    /// `3p_aix_portableminigun_*.baf`) spells out each bone's name instead of indexing into a
    /// companion `.ske`; everything after the name table is byte-identical to version 4.
    #[test]
    fn parses_version_3_by_bone_name() {
        let skeleton = Skeleton {
            bones: vec![
                Bone {
                    name: "root".into(),
                    parent: None,
                    rotation: [0.0, 0.0, 0.0, 1.0],
                    translation: [0.0; 3],
                },
                Bone {
                    name: "Camerabone".into(),
                    parent: Some(0),
                    rotation: [0.0, 0.0, 0.0, 1.0],
                    translation: [0.0; 3],
                },
            ],
        };

        let name = |s: &str| {
            let mut bytes = s.as_bytes().to_vec();
            bytes.push(0);
            let mut out = (bytes.len() as u16).to_le_bytes().to_vec();
            out.extend(bytes);
            out
        };

        let mut data = Vec::new();
        data.extend(3u32.to_le_bytes());
        data.extend(3u16.to_le_bytes()); // bone_count: one matches, two don't
        data.extend(name("Camerabone")); // -> skeleton index 1
        data.extend(name("geom0")); // the weapon's own rigid-part node: no matching bone
        data.extend(name("root")); // -> skeleton index 0
        data.extend(1u32.to_le_bytes()); // frame_count
        data.push(0); // precision
        data.extend(bone_data());
        data.extend(bone_data());
        data.extend(bone_data());

        let anim = Animation::parse(&data, Some(&skeleton)).unwrap();
        // The unmatched `geom0` track is dropped, but its bytes were still consumed (the file
        // parses to completion), and the two skeleton bones are kept in file order.
        assert_eq!(anim.tracks.iter().map(|t| t.bone).collect::<Vec<_>>(), vec![1, 0]);
    }

    #[test]
    fn version_3_without_a_skeleton_is_an_error() {
        let mut data = Vec::new();
        data.extend(3u32.to_le_bytes());
        data.extend(0u16.to_le_bytes());
        data.extend(1u32.to_le_bytes());
        data.push(0);
        assert!(Animation::parse(&data, None).is_err());
    }

    #[test]
    fn other_versions_are_rejected() {
        let data = 5u32.to_le_bytes().to_vec();
        assert!(Animation::parse(&data, None).is_err());
    }
}
