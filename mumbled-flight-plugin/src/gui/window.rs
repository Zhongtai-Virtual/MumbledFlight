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


//! XPLM floating-window creation and C event callbacks.
//!
//! All three windows (main, file-picker, TOFU) share the same five C callbacks.
//! The draw callback dispatches via `GuiState::draw_any`; the input callbacks
//! dispatch via `GuiState::on_any_*`, which resolves the target `ImguiWindowState`
//! from the `win` parameter.

use std::ffi::CStr;
use std::mem;
use std::os::raw::{c_char, c_int, c_void};
use xplane_sys::{
    XPLMCreateWindowEx, XPLMMouseStatus, XPLMSetWindowPositioningMode, XPLMSetWindowTitle,
    XPLMWindowDecoration, XPLMWindowID, XPLMWindowLayer, XPLMWindowPositioningMode,
};

// ── Shared C callbacks (used by all three windows) ────────────────────────────

unsafe extern "C-unwind" fn draw_any_cb(win: XPLMWindowID, _: *mut c_void) {
    let Ok(mut g) = crate::plugin_cell().lock() else { return };
    if let Some(ps) = g.as_mut() {
        ps.gui.draw_any(win);
    }
}

unsafe extern "C-unwind" fn mouse_cb(
    win: XPLMWindowID,
    x: c_int,
    y: c_int,
    s: XPLMMouseStatus,
    _: *mut c_void,
) -> c_int {
    let Ok(mut g) = crate::plugin_cell().lock() else { return 1 };
    if let Some(ps) = g.as_mut() {
        ps.gui.on_any_mouse(win, x, y, s);
    }
    1
}

unsafe extern "C-unwind" fn cursor_cb(
    win: XPLMWindowID,
    x: c_int,
    y: c_int,
    _: *mut c_void,
) -> xplane_sys::XPLMCursorStatus {
    let Ok(mut g) = crate::plugin_cell().lock() else {
        return xplane_sys::XPLMCursorStatus::Default;
    };
    if let Some(ps) = g.as_mut() {
        ps.gui.on_any_mouse_move(win, x, y);
    }
    xplane_sys::XPLMCursorStatus::Default
}

unsafe extern "C-unwind" fn wheel_cb(
    win: XPLMWindowID,
    x: c_int,
    y: c_int,
    wheel: c_int,
    clicks: c_int,
    _: *mut c_void,
) -> c_int {
    let Ok(mut g) = crate::plugin_cell().lock() else { return 1 };
    if let Some(ps) = g.as_mut() {
        ps.gui.on_any_wheel(win, x, y, wheel, clicks);
    }
    1
}

unsafe extern "C-unwind" fn key_cb(
    win: XPLMWindowID,
    key: c_char,
    flags: xplane_sys::XPLMKeyFlags,
    _vk: c_char,
    _: *mut c_void,
    losing: c_int,
) {
    if losing != 0 || (flags & xplane_sys::XPLMKeyFlags::Down).0 == 0 {
        return;
    }
    if key > 0 {
        let Ok(mut g) = crate::plugin_cell().lock() else { return };
        if let Some(ps) = g.as_mut() {
            ps.gui.on_any_char(win, key as u8);
        }
    }
}

// ── Window creation ───────────────────────────────────────────────────────────

unsafe fn make_window(
    left: c_int,
    top: c_int,
    right: c_int,
    bottom: c_int,
    title: &CStr,
) -> XPLMWindowID {
    let mut params = xplane_sys::XPLMCreateWindow_t {
        structSize: mem::size_of::<xplane_sys::XPLMCreateWindow_t>() as c_int,
        left,
        top,
        right,
        bottom,
        visible: 0,
        drawWindowFunc: Some(draw_any_cb),
        handleMouseClickFunc: Some(mouse_cb),
        handleKeyFunc: Some(key_cb),
        handleCursorFunc: Some(cursor_cb),
        handleMouseWheelFunc: Some(wheel_cb),
        refcon: std::ptr::null_mut(),
        decorateAsFloatingWindow: XPLMWindowDecoration::RoundRectangle,
        layer: XPLMWindowLayer::FloatingWindows,
        handleRightClickFunc: None,
    };
    let win = XPLMCreateWindowEx(&mut params);
    XPLMSetWindowTitle(win, title.as_ptr());
    XPLMSetWindowPositioningMode(win, XPLMWindowPositioningMode::PositionFree, -1);
    win
}

pub unsafe fn create_xplm_window() -> XPLMWindowID {
    make_window(60, 660, 690, 60, c"MumbledFlight")
}

pub unsafe fn create_file_picker_window() -> XPLMWindowID {
    make_window(0, 330, 430, 0, c"Browse")
}

pub unsafe fn create_tofu_window() -> XPLMWindowID {
    make_window(0, 280, 430, 0, c"Server Certificate")
}
