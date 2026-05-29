//! Pure spatial-audio math — no I/O, no protocol dependencies.

use audiopus::{coder::Decoder, packet::Packet, MutSignals};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use bytes::Bytes;
use std::io::Cursor;

/// Converts a position between X-Plane (aft-positive Z) and Mumble (forward-positive Z)
/// coordinates. The two systems differ only in the sign of the Z axis, so this is its own
/// inverse — use it at every X-Plane↔Mumble boundary instead of negating Z by hand.
pub const fn xplane_to_mumble(pos: [f32; 3]) -> [f32; 3] {
    [pos[0], pos[1], -pos[2]]
}

/// Mumble's calcGain: maps dot ∈ [-1, 1] → gain ∈ [0.25, 1.0].
pub fn calc_gain(dot: f32) -> f32 {
    let df = (dot + 1.0) * 0.5;
    df + (1.0 - df) * 0.25
}

/// CL650 fuselage AABB in Mumble coordinates (forward-positive Z).
/// Source X-Plane bounds: x[-1.2,1.2] y[-1,1.2] z[-8,1.7]; Z is negated for Mumble.
const FUSELAGE_X_MIN: f32 = -1.2;
const FUSELAGE_X_MAX: f32 =  1.2;
const FUSELAGE_Y_MIN: f32 = -1.0;
const FUSELAGE_Y_MAX: f32 =  1.2;
const FUSELAGE_Z_MIN: f32 = -1.7;  // X-Plane Z=1.7 (nose-ish)
const FUSELAGE_Z_MAX: f32 =  8.0;  // X-Plane Z=-8 (tail-ish)

/// CL650 main entry door position in Mumble coordinates (X-Plane: x=-1, y=0.5, z=-5).
pub const MAIN_DOOR_POS: [f32; 3] = [-1.0, 0.5, 5.0];

pub fn is_inside_aircraft(pos: [f32; 3]) -> bool {
    let [x, y, z] = pos;
    x > FUSELAGE_X_MIN && x < FUSELAGE_X_MAX
        && y > FUSELAGE_Y_MIN && y < FUSELAGE_Y_MAX
        && z > FUSELAGE_Z_MIN && z < FUSELAGE_Z_MAX
}

/// Scalar distance falloff using the same 1.5 m → 8 m curve as `compute_stereo_gains`.
pub(super) fn point_gain(from: [f32; 3], to: [f32; 3]) -> f32 {
    let [fx, fy, fz] = from;
    let [tx, ty, tz] = to;
    let dist = ((tx - fx).powi(2) + (ty - fy).powi(2) + (tz - fz).powi(2)).sqrt();
    const MIN_DIST: f32 = 1.5;
    const MAX_DIST: f32 = 8.0;
    if dist <= MIN_DIST {
        1.0
    } else if dist >= MAX_DIST {
        0.0
    } else {
        let t = 1.0 - (dist - MIN_DIST) / (MAX_DIST - MIN_DIST);
        t * t
    }
}

/// Aircraft skin attenuation for a voice source crossing the fuselage boundary.
///
/// Two acoustic paths are summed:
/// - Hull transmission (always active): `SKIN_BASE` regardless of door state.
/// - Door path (when door is open): step 1 — scalar falloff from source to door aperture,
///   scaled by `door_main`; step 2 — full stereo spatial from door to listener.
///
/// Returns `(left, right)` gain multipliers. When source and listener are on the same side
/// of the fuselage this function is not called — normal `compute_stereo_gains` applies.
pub fn aircraft_skin_gains(
    source_pos:  [f32; 3],
    listener_pos: [f32; 3],
    listener_rot: [f32; 3],
    door:         f32,
    door_lav:     f32,
    door_main:    f32,
) -> (f32, f32) {
    const SKIN_BASE: f32 = 0.15;

    // Path 1: hull transmission — source position, full spatial, attenuated.
    let (hl, hr) = compute_stereo_gains(source_pos, listener_pos, listener_rot, door, door_lav);
    let hull = (hl * SKIN_BASE, hr * SKIN_BASE);

    // Path 2: door aperture — step 1 scalar from source to door, step 2 spatial from door to listener.
    let door_path = if door_main > 0.0 {
        let step1 = point_gain(source_pos, MAIN_DOOR_POS) * door_main;
        let (dl, dr) = compute_stereo_gains(MAIN_DOOR_POS, listener_pos, listener_rot, door, door_lav);
        (step1 * dl, step1 * dr)
    } else {
        (0.0, 0.0)
    };

    (hull.0 + door_path.0, hull.1 + door_path.1)
}

/// Returns 1.0 when source and listener are on the same side of the door,
/// otherwise scales by how open it is.
pub fn door_attenuation(listener_z: f32, source_z: f32, door_z: f32, open: f32, open_threshold: f32) -> f32 {
    if (listener_z - door_z) * (source_z - door_z) >= 0.0 {
        return 1.0;
    }
    0.15 + 0.85 * (open / open_threshold).min(1.0)
}

pub fn decode_opus_packet(decoder: &mut Decoder, data: &[u8]) -> Option<Vec<f32>> {
    let packet = Packet::try_from(data).ok()?;
    let mut pcm = vec![0i16; 5760];
    let len = decoder
        .decode(Some(packet), MutSignals::try_from(pcm.as_mut_slice()).unwrap(), false)
        .ok()?;
    pcm.truncate(len);
    Some(pcm.iter().map(|&x| x as f32 / 32767.0).collect())
}

pub fn parse_position(pos_bytes: Option<Bytes>) -> Option<[f32; 3]> {
    let mut rdr = Cursor::new(pos_bytes?);
    Some([
        rdr.read_f32::<LittleEndian>().ok()?,
        rdr.read_f32::<LittleEndian>().ok()?,
        rdr.read_f32::<LittleEndian>().ok()?,
    ])
}

pub fn encode_pos(x: f32, y: f32, z: f32) -> Bytes {
    let mut buf = Vec::with_capacity(12);
    let _ = buf.write_f32::<LittleEndian>(x);
    let _ = buf.write_f32::<LittleEndian>(y);
    let _ = buf.write_f32::<LittleEndian>(z);
    Bytes::from(buf)
}

/// Computes (left_gain, right_gain) from source/listener positions and head orientation.
pub fn compute_stereo_gains(
    source_pos: [f32; 3],
    listener_pos: [f32; 3],
    listener_rot: [f32; 3],
    door: f32,
    door_lav: f32,
) -> (f32, f32) {
    let [lx, ly, lz] = listener_pos;
    let [sx, sy, sz] = source_pos;
    let [h_psi, h_the, h_phi] = listener_rot;

    let (dx, dy, dz) = (sx - lx, sy - ly, sz - lz);
    let dist = (dx * dx + dy * dy + dz * dz).sqrt();

    if dist < 0.01 {
        return (1.0, 1.0);
    }

    let (nx, ny, nz) = (dx / dist, dy / dist, dz / dist);
    let (hh, hp, hr) = (h_psi.to_radians(), h_the.to_radians(), h_phi.to_radians());

    let fwd = [hp.cos() * hh.sin(), hp.sin(), hp.cos() * hh.cos()];
    let top = [
        -hr.sin() * hh.cos() + hp.sin() * hr.cos() * hh.sin(),
         hp.cos() * hr.cos(),
         hr.sin() * hh.sin()  + hp.sin() * hr.cos() * hh.cos(),
    ];
    let right = [
        top[1] * fwd[2] - top[2] * fwd[1],
        top[2] * fwd[0] - top[0] * fwd[2],
        top[0] * fwd[1] - top[1] * fwd[0],
    ];

    let dot_r = nx * right[0] + ny * right[1] + nz * right[2];

    const MIN_DIST: f32 = 1.5;
    const MAX_DIST: f32 = 8.0;
    let datt = if dist <= MIN_DIST {
        1.0
    } else if dist >= MAX_DIST {
        0.0
    } else {
        let t = 1.0 - (dist - MIN_DIST) / (MAX_DIST - MIN_DIST);
        t * t
    };

    let door_att = door_attenuation(lz, sz, 4.1, door, 0.95)
                 * door_attenuation(lz, sz, -0.43, door_lav, 1.0);

    (calc_gain(-dot_r) * datt * door_att, calc_gain(dot_r) * datt * door_att)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn xplane_mumble_z_negation_is_self_inverse() {
        let xp = [2.0, 0.9, -6.8];
        let mumble = xplane_to_mumble(xp);
        assert_eq!(mumble, [2.0, 0.9, 6.8]);
        // Applying it twice returns the original (it is its own inverse).
        assert_eq!(xplane_to_mumble(mumble), xp);
    }

    #[test]
    fn calc_gain_endpoints() {
        // Mumble's curve maps dot ∈ [-1, 1] → gain ∈ [0.25, 1.0], midpoint 0.625.
        assert!(approx(calc_gain(1.0), 1.0));
        assert!(approx(calc_gain(-1.0), 0.25));
        assert!(approx(calc_gain(0.0), 0.625));
    }

    #[test]
    fn door_attenuation_same_side_is_unity() {
        // Listener and source both forward of the door → no attenuation regardless of openness.
        assert!(approx(door_attenuation(0.0, 1.0, 4.1, 0.0, 0.95), 1.0));
    }

    #[test]
    fn door_attenuation_opposite_sides_scales_with_openness() {
        // Closed door → floor of 0.15; fully open → back to 1.0.
        assert!(approx(door_attenuation(0.0, 10.0, 4.1, 0.0, 0.95), 0.15));
        assert!(approx(door_attenuation(0.0, 10.0, 4.1, 0.95, 0.95), 1.0));
        // Half open (ratio 0.5) → 0.15 + 0.85 * 0.5.
        assert!(approx(door_attenuation(0.0, 10.0, 4.1, 0.475, 0.95), 0.575));
    }

    #[test]
    fn position_round_trips() {
        let bytes = encode_pos(1.5, -2.0, 3.25);
        let parsed = parse_position(Some(bytes)).unwrap();
        assert!(approx(parsed[0], 1.5));
        assert!(approx(parsed[1], -2.0));
        assert!(approx(parsed[2], 3.25));
    }

    #[test]
    fn parse_position_none_is_none() {
        assert!(parse_position(None).is_none());
    }

    #[test]
    fn stereo_gains_pan_toward_source_side() {
        // Listener at origin facing +Z with doors open: head "right" basis vector is +X.
        let listener_pos = [0.0, 0.0, 0.0];
        let listener_rot = [0.0, 0.0, 0.0];

        let (l_right, r_right) = compute_stereo_gains([3.0, 0.0, 0.0], listener_pos, listener_rot, 1.0, 1.0);
        assert!(r_right > l_right, "source on the right should be louder in the right channel");

        let (l_left, r_left) = compute_stereo_gains([-3.0, 0.0, 0.0], listener_pos, listener_rot, 1.0, 1.0);
        assert!(l_left > r_left, "source on the left should be louder in the left channel");
    }

    #[test]
    fn stereo_gains_falloff_with_distance() {
        let listener_pos = [0.0, 0.0, 0.0];
        let listener_rot = [0.0, 0.0, 0.0];

        // Beyond MAX_DIST (8 m) the source is silent.
        let (l, r) = compute_stereo_gains([0.0, 0.0, 20.0], listener_pos, listener_rot, 1.0, 1.0);
        assert!(approx(l, 0.0) && approx(r, 0.0));

        // Co-located (within the 0.01 m guard) → full equal-power output.
        let (l, r) = compute_stereo_gains([0.0, 0.0, 0.005], listener_pos, listener_rot, 1.0, 1.0);
        assert!(approx(l, 1.0) && approx(r, 1.0));
    }

    #[test]
    fn aircraft_aabb_membership() {
        assert!(is_inside_aircraft([0.0, 0.0, 0.0]));
        assert!(is_inside_aircraft(MAIN_DOOR_POS)); // door sits in the hull wall, treated as inside
        // Outside on each axis.
        assert!(!is_inside_aircraft([2.0, 0.0, 0.0]));   // x beyond ±1.2
        assert!(!is_inside_aircraft([0.0, 2.0, 0.0]));   // y beyond +1.2
        assert!(!is_inside_aircraft([0.0, 0.0, 10.0]));  // z beyond +8 (tail)
        assert!(!is_inside_aircraft([0.0, 0.0, -3.0]));  // z beyond -1.7 (nose)
    }

    #[test]
    fn point_gain_curve() {
        // Same 1.5 m → 8 m quadratic falloff as compute_stereo_gains.
        assert!(approx(point_gain([0.0, 0.0, 0.0], [0.0, 0.0, 1.0]), 1.0));   // ≤ 1.5 m
        assert!(approx(point_gain([0.0, 0.0, 0.0], [0.0, 0.0, 10.0]), 0.0));  // ≥ 8 m
        // dist 4.75 m → t = 1 - (4.75-1.5)/6.5 = 0.5 → 0.25
        assert!(approx(point_gain([0.0, 0.0, 0.0], [0.0, 0.0, 4.75]), 0.25));
    }

    #[test]
    fn skin_gains_hull_only_when_main_door_shut() {
        const SKIN_BASE: f32 = 0.15;
        let src = [0.0, 0.0, 0.0];          // inside the fuselage
        let listener = [5.0, 0.0, 0.0];     // outside (x beyond +1.2)
        let rot = [0.0, 0.0, 0.0];

        // With the main door shut, only the hull-transmission path survives:
        // SKIN_BASE × the normal spatial gain.
        let (hl, hr) = compute_stereo_gains(src, listener, rot, 1.0, 1.0);
        let (sl, sr) = aircraft_skin_gains(src, listener, rot, 1.0, 1.0, 0.0);
        assert!(approx(sl, hl * SKIN_BASE));
        assert!(approx(sr, hr * SKIN_BASE));
    }

    #[test]
    fn skin_gains_open_main_door_adds_aperture_path() {
        let src = [0.0, 0.0, 0.0];
        let listener = [5.0, 0.0, 0.0];
        let rot = [0.0, 0.0, 0.0];

        let shut = aircraft_skin_gains(src, listener, rot, 1.0, 1.0, 0.0);
        let open = aircraft_skin_gains(src, listener, rot, 1.0, 1.0, 1.0);
        // The door-aperture path is additive and non-negative, so opening the main door
        // can only raise the gain — and here the positions give a strictly louder result.
        assert!(open.0 > shut.0, "open {} should exceed shut {}", open.0, shut.0);
        assert!(open.1 > shut.1, "open {} should exceed shut {}", open.1, shut.1);
    }
}
