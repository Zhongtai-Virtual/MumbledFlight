// Copyright (C) 2026 Zhongtai Virtual
//
// This file is part of MumbledFlight.
//
// MumbledFlight is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// MumbledFlight is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with MumbledFlight.  If not, see <https://www.gnu.org/licenses/>.


//! Shared state and DataRef management for the MumbledFlight application.

use log::{debug, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CockpitSeat {
    #[default]
    Captain      = 0,
    FirstOfficer = 1,
}

impl CockpitSeat {
    pub fn from_int(n: i32) -> Result<Self, i32> {
        match n {
            0 => Ok(Self::Captain),
            1 => Ok(Self::FirstOfficer),
            _ => Err(n),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SharedCockpitZone {
    #[default]
    InFbo            = 0,
    AroundOrInAircraft = 2,
}

impl SharedCockpitZone {
    pub fn from_int(n: i32) -> Result<Self, i32> {
        match n {
            0 => Ok(Self::InFbo),
            2 => Ok(Self::AroundOrInAircraft),
            _ => Err(n),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AcpMicSelection {
    #[default]
    Vhf1   = 0,
    Vhf2   = 1,
    Vhf3   = 2,
    Hf1    = 3,
    Hf2    = 4,
    IntSvc = 5,
    Pa     = 6,
}

impl AcpMicSelection {
    pub fn from_int(n: i32) -> Result<Self, i32> {
        match n {
            0 => Ok(Self::Vhf1),
            1 => Ok(Self::Vhf2),
            2 => Ok(Self::Vhf3),
            3 => Ok(Self::Hf1),
            4 => Ok(Self::Hf2),
            5 => Ok(Self::IntSvc),
            6 => Ok(Self::Pa),
            _ => Err(n),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SharedCockpitRole {
    #[default]
    Pilot      = 0,
    Jumpseater = 1,
    Pax        = 2,
    Spectator  = 3,
}

impl SharedCockpitRole {
    pub fn from_int(n: i32) -> Result<Self, i32> {
        match n {
            0 => Ok(Self::Pilot),
            1 => Ok(Self::Jumpseater),
            2 => Ok(Self::Pax),
            3 => Ok(Self::Spectator),
            _ => Err(n),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataRefId {
    HeadX, HeadY, HeadZ,
    HeadPsi, HeadThe, HeadPhi,
    PlanePsi,
    PilotSeat,
    SharedCkptRole,
    SharedCkptZone,
    Acp1Ic, Acp1Rt, Acp1Mic, Acp1SpkrTog, Acp1SpkrVol, Acp1IntSvcTog, Acp1IntSvcVol,
    Acp2Ic, Acp2Rt, Acp2Mic, Acp2SpkrTog, Acp2SpkrVol, Acp2IntSvcTog, Acp2IntSvcVol,
    Contwheel0Ic, Contwheel1Ic,
    Contwheel0Rt, Contwheel1Rt,
    DoorCabin,
    DoorLavatory,
    /// CL650/doors/main/door: 0 = closed, 1 = open
    DoorMain,
    /// CL650/shared_ckpt/is_guest: user is a shared-cockpit guest (not the host)
    SharedCkptIsGuest,
    /// xpilot/audio/com1_rx: xPilot COM1 receiver active
    XpilotCom1Rx,
    /// xpilot/audio/com2_rx: xPilot COM2 receiver active
    XpilotCom2Rx,
}

impl DataRefId {
    pub fn name(&self) -> &'static str {
        match self {
            DataRefId::HeadX => "sim/graphics/view/pilots_head_x",
            DataRefId::HeadY => "sim/graphics/view/pilots_head_y",
            DataRefId::HeadZ => "sim/graphics/view/pilots_head_z",
            DataRefId::HeadPsi => "sim/graphics/view/pilots_head_psi",
            DataRefId::HeadThe => "sim/graphics/view/pilots_head_the",
            DataRefId::HeadPhi => "sim/graphics/view/pilots_head_phi",
            DataRefId::PlanePsi => "sim/flightmodel/position/psi",
            DataRefId::PilotSeat => "CL650/pilot_seat",
            DataRefId::SharedCkptRole => "CL650/shared_ckpt/my_role",
            DataRefId::SharedCkptZone => "CL650/shared_ckpt/my_zone",
            DataRefId::Acp1Ic         => "CL650/ACP/1/ic",
            DataRefId::Acp1Rt         => "CL650/ACP/1/rt",
            DataRefId::Acp1Mic        => "CL650/ACP/1/mic_value",
            DataRefId::Acp1SpkrTog    => "CL650/ACP/1/spkr_tog_value",
            DataRefId::Acp1SpkrVol    => "CL650/ACP/1/spkr_vol",
            DataRefId::Acp1IntSvcTog  => "CL650/ACP/1/int_svc_tog_value",
            DataRefId::Acp1IntSvcVol  => "CL650/ACP/1/int_svc_vol",
            DataRefId::Acp2Ic         => "CL650/ACP/2/ic",
            DataRefId::Acp2Rt         => "CL650/ACP/2/rt",
            DataRefId::Acp2Mic        => "CL650/ACP/2/mic_value",
            DataRefId::Acp2SpkrTog    => "CL650/ACP/2/spkr_tog_value",
            DataRefId::Acp2SpkrVol    => "CL650/ACP/2/spkr_vol",
            DataRefId::Acp2IntSvcTog  => "CL650/ACP/2/int_svc_tog_value",
            DataRefId::Acp2IntSvcVol  => "CL650/ACP/2/int_svc_vol",
            DataRefId::Contwheel0Ic   => "CL650/contwheel/0/ic",
            DataRefId::Contwheel1Ic   => "CL650/contwheel/1/ic",
            DataRefId::Contwheel0Rt   => "CL650/contwheel/0/rt",
            DataRefId::Contwheel1Rt   => "CL650/contwheel/1/rt",
            DataRefId::DoorCabin         => "CL650/doors/cabin/door",
            DataRefId::DoorLavatory      => "CL650/doors/cabin/lavatory",
            DataRefId::DoorMain          => "CL650/doors/main/door",
            DataRefId::SharedCkptIsGuest => "CL650/shared_ckpt/is_guest",
            DataRefId::XpilotCom1Rx      => "xpilot/audio/com1_rx",
            DataRefId::XpilotCom2Rx      => "xpilot/audio/com2_rx",
        }
    }

    pub fn all() -> &'static [DataRefId] {
        &[
            DataRefId::HeadX, DataRefId::HeadY, DataRefId::HeadZ,
            DataRefId::HeadPsi, DataRefId::HeadThe, DataRefId::HeadPhi,
            DataRefId::PlanePsi,
            DataRefId::PilotSeat,
            DataRefId::SharedCkptRole,
            DataRefId::SharedCkptZone,
            DataRefId::Acp1Ic, DataRefId::Acp1Rt, DataRefId::Acp1Mic,
            DataRefId::Acp1SpkrTog, DataRefId::Acp1SpkrVol,
            DataRefId::Acp1IntSvcTog, DataRefId::Acp1IntSvcVol,
            DataRefId::Acp2Ic, DataRefId::Acp2Rt, DataRefId::Acp2Mic,
            DataRefId::Acp2SpkrTog, DataRefId::Acp2SpkrVol,
            DataRefId::Acp2IntSvcTog, DataRefId::Acp2IntSvcVol,
            DataRefId::Contwheel0Ic, DataRefId::Contwheel1Ic,
            DataRefId::Contwheel0Rt, DataRefId::Contwheel1Rt,
            DataRefId::DoorCabin,
            DataRefId::DoorLavatory,
            DataRefId::DoorMain,
            DataRefId::SharedCkptIsGuest,
            DataRefId::XpilotCom1Rx,
            DataRefId::XpilotCom2Rx,
        ]
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::all().iter().find(|id| id.name() == name).copied()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CockpitState {
    pub pos: [f32; 3],
    pub rot: [f32; 3],
    pub plane_rot: [f32; 3],
    pub seat: CockpitSeat,
    pub role: SharedCockpitRole,
    pub zone: SharedCockpitZone,
    /// CL650/ACP/*/ic: ACP interphone push-to-talk
    pub acp_ic: bool,
    /// CL650/contwheel/*/ic: control wheel IC push-to-talk
    pub contwheel_ic: bool,
    /// CL650/ACP/*/rt: ACP radio transmit push-to-talk
    pub acp_rt: bool,
    /// CL650/contwheel/*/rt: control wheel radio transmit push-to-talk
    pub contwheel_rt: bool,
    pub mic: AcpMicSelection,
    /// CL650/ACP/*/spkr_tog_value: effective speaker on/off
    pub spkr_tog: bool,
    /// CL650/ACP/*/spkr_vol: speaker playback volume 0.0 – 1.0
    pub spkr_vol: f32,
    /// CL650/ACP/*/int_svc_tog_value: IC playback speaker on/off
    pub ic_tog: bool,
    /// CL650/ACP/*/int_svc_vol: IC playback volume 0.0 (silence) – 1.0 (full)
    pub ic_vol: f32,
    /// CL650/doors/cabin/door: 0.0 = closed, 0.95 = panel removed, 1.0 = stored
    pub door: f32,
    /// CL650/doors/cabin/lavatory: 0.0 = closed, 1.0 = open
    pub door_lav: f32,
    /// CL650/doors/main/door: 0.0 = closed, 1.0 = open
    pub door_main: f32,
    /// xpilot/audio/com1_rx: xPilot COM1 receiver active
    pub com1_rx: bool,
    /// xpilot/audio/com2_rx: xPilot COM2 receiver active
    pub com2_rx: bool,
    /// CL650/shared_ckpt/is_guest: user is a shared-cockpit guest
    pub is_guest: bool,
}

impl Default for CockpitState {
    fn default() -> Self {
        Self {
            pos: [0.0; 3],
            rot: [0.0; 3],
            plane_rot: [0.0; 3],
            seat: CockpitSeat::Captain,
            role: SharedCockpitRole::Pilot,
            zone: SharedCockpitZone::InFbo,
            acp_ic: false,
            contwheel_ic: false,
            acp_rt: false,
            contwheel_rt: false,
            mic: AcpMicSelection::Vhf1,
            spkr_tog: false,
            spkr_vol: 0.0,
            ic_tog: false,
            ic_vol: 0.0,
            door: 1.0,      // open by default — no spurious attenuation before DataRefs are read
            door_lav: 1.0,
            door_main: 1.0,
            com1_rx: false,
            com2_rx: false,
            is_guest: false,
        }
    }
}

impl CockpitState {
    /// DataRef booleans are encoded as 0.0/1.0; treat anything meaningfully above zero as true.
    pub fn f32_to_bool(v: f32) -> bool {
        v > 0.1
    }

    /// DataRef enums arrive as whole numbers; round to absorb float representation error.
    fn f32_to_int(v: f32) -> i32 {
        v.round() as i32
    }

    /// Whether a seat-specific DataRef belongs to the seat currently occupied. ACP1 and
    /// contwheel0 are the Captain's (left seat); ACP2 and contwheel1 the First Officer's
    /// (right seat). This is the one place that resolves "which physical control is mine."
    /// Non-seat-specific DataRefs always apply, so they are not listed here.
    fn owns(&self, id: DataRefId) -> bool {
        use DataRefId::*;
        let is_left = self.seat == CockpitSeat::Captain;
        match id {
            // Captain (left seat) controls
            Acp1Ic | Acp1Rt | Acp1Mic | Acp1SpkrTog | Acp1SpkrVol
            | Acp1IntSvcTog | Acp1IntSvcVol | Contwheel0Ic | Contwheel0Rt => is_left,
            // First Officer (right seat) controls
            Acp2Ic | Acp2Rt | Acp2Mic | Acp2SpkrTog | Acp2SpkrVol
            | Acp2IntSvcTog | Acp2IntSvcVol | Contwheel1Ic | Contwheel1Rt => !is_left,
            // Non-seat-specific — always apply
            HeadX | HeadY | HeadZ | HeadPsi | HeadThe | HeadPhi | PlanePsi
            | PilotSeat | SharedCkptRole | SharedCkptZone
            | DoorCabin | DoorLavatory | DoorMain | SharedCkptIsGuest
            | XpilotCom1Rx | XpilotCom2Rx => true,
        }
    }

    /// Whether the IC client should transmit: seated as Pilot, IC keyed, RT not active.
    pub fn should_transmit_ic(&self) -> bool {
        self.role == SharedCockpitRole::Pilot
            && (self.acp_ic || self.contwheel_ic)
            && !self.acp_rt
            && !self.contwheel_rt
    }

    /// Whether the PA client should transmit: seated as Pilot, mic selector on PA, RT active.
    pub fn should_transmit_pa(&self) -> bool {
        self.role == SharedCockpitRole::Pilot
            && self.mic == AcpMicSelection::Pa
            && (self.acp_rt || self.contwheel_rt)
    }

    /// Whether the Radio relay client should transmit.
    pub fn should_transmit_radio(&self, has_source: bool) -> bool {
        has_source && !self.is_guest && (self.com1_rx || self.com2_rx) && self.spkr_tog
    }

    /// Applies a raw f32 DataRef value to the cockpit state. This is the single update path:
    /// the plugin reads f32/i32 XPLM handles directly, and the CLI bridge converts its JSON
    /// values to f32 at the bridge boundary (see `cli/src/xplane/bridge.rs`).
    pub fn update_from_float(&mut self, id: DataRefId, val: f32) {
        let old = (self.acp_ic, self.contwheel_ic, self.mic, self.seat, self.role, self.zone);

        // Seat-specific controls (ACP*, contwheel*) only apply when `owns(id)` is true.
        let mine = self.owns(id);

        match id {
            DataRefId::HeadX => self.pos[0] = val,
            DataRefId::HeadY => self.pos[1] = val,
            DataRefId::HeadZ => self.pos[2] = val,
            DataRefId::HeadPsi => self.rot[0] = val,
            DataRefId::HeadThe => self.rot[1] = val,
            DataRefId::HeadPhi => self.rot[2] = val,
            DataRefId::PlanePsi => self.plane_rot[0] = val,
            DataRefId::PilotSeat => match CockpitSeat::from_int(Self::f32_to_int(val)) {
                Ok(s)  => self.seat = s,
                Err(n) => warn!("[State] unknown pilot seat value {n}"),
            },
            DataRefId::SharedCkptRole => match SharedCockpitRole::from_int(Self::f32_to_int(val)) {
                Ok(r)  => self.role = r,
                Err(n) => warn!("[State] unknown shared cockpit role value {n}"),
            },
            DataRefId::SharedCkptZone => match SharedCockpitZone::from_int(Self::f32_to_int(val)) {
                Ok(z)  => self.zone = z,
                Err(n) => warn!("[State] unknown shared cockpit zone value {n}"),
            },

            DataRefId::Acp1Ic        | DataRefId::Acp2Ic        => if mine { self.acp_ic   = Self::f32_to_bool(val) },
            DataRefId::Acp1Rt        | DataRefId::Acp2Rt        => if mine { self.acp_rt   = Self::f32_to_bool(val) },
            DataRefId::Acp1Mic       | DataRefId::Acp2Mic       => if mine {
                match AcpMicSelection::from_int(Self::f32_to_int(val)) {
                    Ok(m)  => self.mic = m,
                    Err(n) => warn!("[State] unknown ACP mic value {n}"),
                }
            },
            DataRefId::Acp1SpkrTog   | DataRefId::Acp2SpkrTog   => if mine { self.spkr_tog = Self::f32_to_bool(val) },
            DataRefId::Acp1SpkrVol   | DataRefId::Acp2SpkrVol   => if mine { self.spkr_vol = val },
            DataRefId::Acp1IntSvcTog | DataRefId::Acp2IntSvcTog => if mine { self.ic_tog   = Self::f32_to_bool(val) },
            DataRefId::Acp1IntSvcVol | DataRefId::Acp2IntSvcVol => if mine { self.ic_vol   = val },
            DataRefId::Contwheel0Ic  | DataRefId::Contwheel1Ic  => if mine { self.contwheel_ic = Self::f32_to_bool(val) },
            DataRefId::Contwheel0Rt  | DataRefId::Contwheel1Rt  => if mine { self.contwheel_rt = Self::f32_to_bool(val) },

            DataRefId::DoorCabin    => self.door      = val,
            DataRefId::DoorLavatory => self.door_lav  = val,
            DataRefId::DoorMain     => self.door_main = val,
            DataRefId::XpilotCom1Rx     => self.com1_rx  = Self::f32_to_bool(val),
            DataRefId::XpilotCom2Rx     => self.com2_rx  = Self::f32_to_bool(val),
            DataRefId::SharedCkptIsGuest => self.is_guest = Self::f32_to_bool(val),
        }

        let new = (self.acp_ic, self.contwheel_ic, self.mic, self.seat, self.role, self.zone);
        if new != old {
            debug!("[State] seat={:?} role={:?} zone={:?} ic={} contwheel_ic={} mic={:?} (via {:?})",
                self.seat, self.role, self.zone, self.acp_ic, self.contwheel_ic, self.mic, id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_from_int_boundaries() {
        assert_eq!(CockpitSeat::from_int(0), Ok(CockpitSeat::Captain));
        assert_eq!(CockpitSeat::from_int(1), Ok(CockpitSeat::FirstOfficer));
        assert_eq!(CockpitSeat::from_int(2), Err(2));

        // Zone uses a non-contiguous mapping (0 and 2); 1 is invalid.
        assert_eq!(SharedCockpitZone::from_int(0), Ok(SharedCockpitZone::InFbo));
        assert_eq!(SharedCockpitZone::from_int(2), Ok(SharedCockpitZone::AroundOrInAircraft));
        assert_eq!(SharedCockpitZone::from_int(1), Err(1));

        assert_eq!(SharedCockpitRole::from_int(3), Ok(SharedCockpitRole::Spectator));
        assert_eq!(SharedCockpitRole::from_int(4), Err(4));

        assert_eq!(AcpMicSelection::from_int(6), Ok(AcpMicSelection::Pa));
        assert_eq!(AcpMicSelection::from_int(7), Err(7));
    }

    #[test]
    fn captain_owns_acp1_ignores_acp2() {
        let mut cs = CockpitState::default(); // seat defaults to Captain (left)
        cs.update_from_float(DataRefId::Acp1Ic, 1.0);
        assert!(cs.acp_ic, "ACP1 IC should apply in the left seat");

        cs.update_from_float(DataRefId::Acp2Ic, 0.0);
        assert!(cs.acp_ic, "ACP2 IC must not touch state while in the left seat");
    }

    #[test]
    fn first_officer_owns_acp2_ignores_acp1() {
        let mut cs = CockpitState::default();
        cs.update_from_float(DataRefId::PilotSeat, 1.0); // move to right seat
        assert_eq!(cs.seat, CockpitSeat::FirstOfficer);

        cs.update_from_float(DataRefId::Acp1Ic, 1.0);
        assert!(!cs.acp_ic, "ACP1 IC must not apply in the right seat");

        cs.update_from_float(DataRefId::Acp2Ic, 1.0);
        assert!(cs.acp_ic, "ACP2 IC should apply in the right seat");
    }

    #[test]
    fn contwheel_follows_seat() {
        let mut cs = CockpitState::default();
        cs.update_from_float(DataRefId::Contwheel0Rt, 1.0);
        assert!(cs.contwheel_rt, "contwheel 0 belongs to the captain");

        cs.update_from_float(DataRefId::PilotSeat, 1.0);
        cs.update_from_float(DataRefId::Contwheel0Rt, 0.0); // captain wheel ignored now
        assert!(cs.contwheel_rt, "captain contwheel must not clear FO state");
        cs.update_from_float(DataRefId::Contwheel1Rt, 0.0);
        assert!(!cs.contwheel_rt, "FO contwheel 1 should apply in the right seat");
    }

    #[test]
    fn bool_threshold_and_scalar_passthrough() {
        let mut cs = CockpitState::default();
        // Booleans use a >0.1 threshold.
        cs.update_from_float(DataRefId::XpilotCom1Rx, 0.05);
        assert!(!cs.com1_rx);
        cs.update_from_float(DataRefId::XpilotCom1Rx, 1.0);
        assert!(cs.com1_rx);

        // Scalars (volumes, door positions) pass through verbatim.
        cs.update_from_float(DataRefId::Acp1SpkrVol, 0.73);
        assert_eq!(cs.spkr_vol, 0.73);
        cs.update_from_float(DataRefId::DoorCabin, 0.0);
        assert_eq!(cs.door, 0.0);
    }

    #[test]
    fn enum_dataref_rounds_before_mapping() {
        let mut cs = CockpitState::default();
        // Float-encoded enum values must round, not truncate (0.999 → 1, not 0).
        cs.update_from_float(DataRefId::SharedCkptZone, 1.999);
        assert_eq!(cs.zone, SharedCockpitZone::AroundOrInAircraft);
    }

    #[test]
    fn dataref_names_unique_and_round_trip() {
        // Guards the "add a DataRef in three places" convention: every variant listed in
        // all() must have a unique name() that round-trips through from_name().
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for &id in DataRefId::all() {
            let name = id.name();
            assert!(seen.insert(name), "duplicate DataRef name: {name}");
            assert_eq!(DataRefId::from_name(name), Some(id), "from_name failed for {name}");
        }
        // The fuselage skin model's main-door DataRef is registered.
        assert!(DataRefId::all().contains(&DataRefId::DoorMain));
    }

    #[test]
    fn door_main_updates_and_defaults_open() {
        let mut cs = CockpitState::default();
        assert_eq!(cs.door_main, 1.0); // open by default — no spurious attenuation pre-DataRef
        cs.update_from_float(DataRefId::DoorMain, 0.0);
        assert_eq!(cs.door_main, 0.0);
    }
}
