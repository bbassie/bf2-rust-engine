//! BF2 → engine coordinate conversion.
//!
//! BF2 is left-handed (+X east/right, +Y up, +Z north/forward). The engine is right-handed
//! with -Z forward. Mirroring Z maps one onto the other while keeping +X and +Y:
//! positions become `(x, y, -z)` and triangle winding flips.

use glam::{Mat4, Quat, Vec3, Vec4};

pub fn position(p: [f32; 3]) -> [f32; 3] {
    [p[0], p[1], -p[2]]
}

pub fn direction(d: [f32; 3]) -> [f32; 3] {
    position(d)
}

/// `yaw/pitch/roll` in degrees (as in `Object.rotation` and `ObjectTemplate.setRotation`).
/// +yaw turns north towards east, +pitch tilts the nose down, +roll lifts the right side.
pub fn rotation_ypr(ypr_degrees: [f32; 3]) -> Quat {
    let [yaw, pitch, roll] = ypr_degrees.map(f32::to_radians);
    Quat::from_rotation_y(-yaw) * Quat::from_rotation_x(-pitch) * Quat::from_rotation_z(roll)
}

/// A BF2 row-vector matrix (local axes as rows, translation in row 3) to scale, rotation,
/// translation in engine space.
pub fn matrix(rows: [[f32; 4]; 4]) -> (Vec3, Quat, Vec3) {
    // Reading the rows as columns transposes into column-vector form.
    let m = Mat4::from_cols_array_2d(&rows);
    let s = Mat4::from_diagonal(Vec4::new(1.0, 1.0, -1.0, 1.0));
    (s * m * s).to_scale_rotation_translation()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaw_90_faces_east() {
        let forward = rotation_ypr([90.0, 0.0, 0.0]) * Vec3::NEG_Z;
        assert!((forward - Vec3::X).length() < 1e-5, "{forward}");
    }

    #[test]
    fn positive_pitch_tilts_down() {
        let forward = rotation_ypr([0.0, 30.0, 0.0]) * Vec3::NEG_Z;
        assert!(forward.y < -0.4, "{forward}");
    }

    #[test]
    fn pure_yaw_matrix_matches_ypr() {
        // BF2 pure-yaw rows: [c 0 -s][0 1 0][s 0 c]
        let a = 35f32.to_radians();
        let (c, s) = (a.cos(), a.sin());
        let rows = [
            [c, 0.0, -s, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [s, 0.0, c, 0.0],
            [1.0, 2.0, 3.0, 1.0],
        ];
        let (_, rotation, translation) = matrix(rows);
        let expected = rotation_ypr([35.0, 0.0, 0.0]);
        for v in [Vec3::X, Vec3::Y, Vec3::NEG_Z] {
            let (a, b) = (rotation * v, expected * v);
            assert!((a - b).length() < 1e-5, "{v}: {a} vs {b}");
        }
        assert_eq!(translation, Vec3::new(1.0, 2.0, -3.0));
    }
}
