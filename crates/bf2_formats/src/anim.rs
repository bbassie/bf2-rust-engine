//! Skeletons (`.ske`, version 2) and animations (`.baf`, version 4).
//!
//! Both store the *conjugate* of each bone's local rotation; [`Skeleton`] and [`Animation`]
//! return the true rotation. Transforms are parent-relative, in BF2's left-handed space.

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

    pub fn parse(data: &[u8]) -> Result<Self, ReadError> {
        let mut r = Reader::new(data);
        let version = r.u32()?;
        if version != 4 {
            return Err(ReadError::Invalid(format!("unsupported animation version {version}")));
        }
        let bone_count = r.u16()? as usize;
        let bone_ids: Vec<usize> = (0..bone_count)
            .map(|_| r.u16().map(|v| v as usize))
            .collect::<Result<_, _>>()?;
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
