//! Pure spatial-audio math — no I/O, no protocol dependencies.

use audiopus::{coder::Decoder, packet::Packet, MutSignals};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use bytes::Bytes;
use std::io::Cursor;

/// Mumble's calcGain: maps dot ∈ [-1, 1] → gain ∈ [0.25, 1.0].
pub fn calc_gain(dot: f32) -> f32 {
    let df = (dot + 1.0) * 0.5;
    df + (1.0 - df) * 0.25
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
