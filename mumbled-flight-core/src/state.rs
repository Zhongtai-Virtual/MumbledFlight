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
    Acp1Ic, Acp1Mic, Acp1Spkr,
    Acp2Ic, Acp2Mic, Acp2Spkr,
    DoorCabin,
    DoorLavatory,
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
            DataRefId::Acp1Ic => "CL650/ACP/1/ic",
            DataRefId::Acp1Mic => "CL650/ACP/1/mic_value",
            DataRefId::Acp1Spkr => "CL650/ACP/1/spkr_tog",
            DataRefId::Acp2Ic => "CL650/ACP/2/ic",
            DataRefId::Acp2Mic => "CL650/ACP/2/mic_value",
            DataRefId::Acp2Spkr => "CL650/ACP/2/spkr_tog",
            DataRefId::DoorCabin => "CL650/doors/cabin/door",
            DataRefId::DoorLavatory => "CL650/doors/cabin/lavatory",
        }
    }

    pub fn all() -> &'static [DataRefId] {
        &[
            DataRefId::HeadX, DataRefId::HeadY, DataRefId::HeadZ,
            DataRefId::HeadPsi, DataRefId::HeadThe, DataRefId::HeadPhi,
            DataRefId::PlanePsi,
            DataRefId::PilotSeat,
            DataRefId::SharedCkptRole,
            DataRefId::Acp1Ic, DataRefId::Acp1Mic, DataRefId::Acp1Spkr,
            DataRefId::Acp2Ic, DataRefId::Acp2Mic, DataRefId::Acp2Spkr,
            DataRefId::DoorCabin,
            DataRefId::DoorLavatory,
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
    pub ic: bool,
    pub pa: bool,
    pub spkr: bool,
    /// CL650/doors/cabin/door: 0.0 = closed, 0.95 = panel removed, 1.0 = stored
    pub door: f32,
    /// CL650/doors/cabin/lavatory: 0.0 = closed, 1.0 = open
    pub door_lav: f32,
}

impl Default for CockpitState {
    fn default() -> Self {
        Self {
            pos: [0.0; 3],
            rot: [0.0; 3],
            plane_rot: [0.0; 3],
            seat: CockpitSeat::Captain,
            role: SharedCockpitRole::Pilot,
            ic: false,
            pa: false,
            spkr: false,
            door: 1.0,      // open by default — no spurious attenuation before DataRefs are read
            door_lav: 1.0,
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
        let is_pilot = self.seat == CockpitSeat::Captain;
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

            DataRefId::Acp1Ic => if is_pilot { self.ic = Self::val_to_bool(val) },
            DataRefId::Acp1Mic => if is_pilot { self.pa = Self::val_to_int(val) == 7 },
            DataRefId::Acp1Spkr => if is_pilot { self.spkr = Self::val_to_bool(val) },
            
            DataRefId::Acp2Ic => if !is_pilot { self.ic = Self::val_to_bool(val) },
            DataRefId::Acp2Mic => if !is_pilot { self.pa = Self::val_to_int(val) == 7 },
            DataRefId::Acp2Spkr => if !is_pilot { self.spkr = Self::val_to_bool(val) },
            DataRefId::DoorCabin => self.door = val.as_f64().unwrap_or(1.0) as f32,
            DataRefId::DoorLavatory => self.door_lav = val.as_f64().unwrap_or(1.0) as f32,
        }

        if self.ic != old_state.ic || self.pa != old_state.pa || self.seat != old_state.seat || self.role != old_state.role {
            debug!("[State] seat={:?} role={:?} ic={} pa={} (via {:?})", self.seat, self.role, self.ic, self.pa, id);
        }
    }

    /// Convenience wrapper for the plugin flight loop: update from a raw f32 DataRef value.
    pub fn update_from_float(&mut self, id: DataRefId, val: f32) {
        self.update_from_dataref(id, &serde_json::json!(val as f64));
    }
}
