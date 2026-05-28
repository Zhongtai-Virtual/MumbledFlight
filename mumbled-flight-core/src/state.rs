//! Shared state and DataRef management for the MumbledFlight application.

use serde_json::Value;
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
            com1_rx: false,
            com2_rx: false,
            is_guest: false,
        }
    }
}

impl CockpitState {
    fn val_to_bool(val: &Value) -> bool {
        if let Some(f) = val.as_f64() {
            f > 0.1
        } else if let Some(i) = val.as_i64() {
            i != 0
        } else {
            false
        }
    }

    fn val_to_int(val: &Value) -> i32 {
        val.as_i64().map(|i| i as i32)
           .unwrap_or_else(|| val.as_f64().map(|f| f as i32).unwrap_or(0))
    }

    pub fn update_from_dataref(&mut self, id: DataRefId, val: &Value) {
        let is_left_seat = self.seat == CockpitSeat::Captain; // pilot_seat 0 = left, 1 = right
        let old_state = self.clone();

        match id {
            DataRefId::HeadX => self.pos[0] = val.as_f64().unwrap_or(0.0) as f32,
            DataRefId::HeadY => self.pos[1] = val.as_f64().unwrap_or(0.0) as f32,
            DataRefId::HeadZ => self.pos[2] = val.as_f64().unwrap_or(0.0) as f32,
            DataRefId::HeadPsi => self.rot[0] = val.as_f64().unwrap_or(0.0) as f32,
            DataRefId::HeadThe => self.rot[1] = val.as_f64().unwrap_or(0.0) as f32,
            DataRefId::HeadPhi => self.rot[2] = val.as_f64().unwrap_or(0.0) as f32,
            DataRefId::PlanePsi => self.plane_rot[0] = val.as_f64().unwrap_or(0.0) as f32,
            DataRefId::PilotSeat => match CockpitSeat::from_int(Self::val_to_int(val)) {
                Ok(s)  => self.seat = s,
                Err(n) => warn!("[State] unknown pilot seat value {n}"),
            },
            DataRefId::SharedCkptRole => match SharedCockpitRole::from_int(Self::val_to_int(val)) {
                Ok(r)  => self.role = r,
                Err(n) => warn!("[State] unknown shared cockpit role value {n}"),
            },
            DataRefId::SharedCkptZone => match SharedCockpitZone::from_int(Self::val_to_int(val)) {
                Ok(z)  => self.zone = z,
                Err(n) => warn!("[State] unknown shared cockpit zone value {n}"),
            },

            DataRefId::Acp1Ic          => if  is_left_seat { self.acp_ic = Self::val_to_bool(val) },
            DataRefId::Acp1Rt          => if  is_left_seat { self.acp_rt = Self::val_to_bool(val) },
            DataRefId::Acp1Mic         => if is_left_seat {
                match AcpMicSelection::from_int(Self::val_to_int(val)) {
                    Ok(m)  => self.mic = m,
                    Err(n) => warn!("[State] unknown ACP1 mic value {n}"),
                }
            },
            DataRefId::Acp1SpkrTog     => if is_left_seat { self.spkr_tog = Self::val_to_bool(val) },
            DataRefId::Acp1SpkrVol     => if is_left_seat { self.spkr_vol = val.as_f64().unwrap_or(0.0) as f32 },
            DataRefId::Acp1IntSvcTog   => if is_left_seat { self.ic_tog   = Self::val_to_bool(val) },
            DataRefId::Acp1IntSvcVol   => if is_left_seat { self.ic_vol   = val.as_f64().unwrap_or(0.0) as f32 },

            DataRefId::Acp2Ic          => if !is_left_seat { self.acp_ic = Self::val_to_bool(val) },
            DataRefId::Acp2Rt          => if !is_left_seat { self.acp_rt = Self::val_to_bool(val) },
            DataRefId::Acp2Mic         => if !is_left_seat {
                match AcpMicSelection::from_int(Self::val_to_int(val)) {
                    Ok(m)  => self.mic = m,
                    Err(n) => warn!("[State] unknown ACP2 mic value {n}"),
                }
            },
            DataRefId::Acp2SpkrTog     => if !is_left_seat { self.spkr_tog = Self::val_to_bool(val) },
            DataRefId::Acp2SpkrVol     => if !is_left_seat { self.spkr_vol = val.as_f64().unwrap_or(0.0) as f32 },
            DataRefId::Acp2IntSvcTog   => if !is_left_seat { self.ic_tog   = Self::val_to_bool(val) },
            DataRefId::Acp2IntSvcVol   => if !is_left_seat { self.ic_vol   = val.as_f64().unwrap_or(0.0) as f32 },
            DataRefId::Contwheel0Ic    => if  is_left_seat { self.contwheel_ic = Self::val_to_bool(val) },
            DataRefId::Contwheel1Ic    => if !is_left_seat { self.contwheel_ic = Self::val_to_bool(val) },
            DataRefId::Contwheel0Rt    => if  is_left_seat { self.contwheel_rt = Self::val_to_bool(val) },
            DataRefId::Contwheel1Rt    => if !is_left_seat { self.contwheel_rt = Self::val_to_bool(val) },
            DataRefId::DoorCabin    => self.door     = val.as_f64().unwrap_or(1.0) as f32,
            DataRefId::DoorLavatory => self.door_lav = val.as_f64().unwrap_or(1.0) as f32,
            DataRefId::XpilotCom1Rx     => self.com1_rx  = Self::val_to_bool(val),
            DataRefId::XpilotCom2Rx     => self.com2_rx  = Self::val_to_bool(val),
            DataRefId::SharedCkptIsGuest => self.is_guest = Self::val_to_bool(val),
        }

        if self.acp_ic != old_state.acp_ic || self.contwheel_ic != old_state.contwheel_ic
            || self.mic != old_state.mic
            || self.seat != old_state.seat || self.role != old_state.role || self.zone != old_state.zone
        {
            debug!("[State] seat={:?} role={:?} zone={:?} ic={} contwheel_ic={} mic={:?} (via {:?})",
                self.seat, self.role, self.zone, self.acp_ic, self.contwheel_ic, self.mic, id);
        }
    }

    /// Convenience wrapper for the plugin flight loop: update from a raw f32 DataRef value.
    pub fn update_from_float(&mut self, id: DataRefId, val: f32) {
        self.update_from_dataref(id, &serde_json::json!(val as f64));
    }
}
