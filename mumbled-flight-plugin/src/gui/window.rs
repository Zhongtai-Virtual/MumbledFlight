//! XPLM floating-window creation and C event callbacks.

use std::os::raw::{c_char, c_int, c_void};
use xplane_sys::{
    XPLMCreateWindowEx, XPLMMouseStatus, XPLMSetWindowPositioningMode, XPLMSetWindowTitle,
    XPLMWindowDecoration, XPLMWindowID, XPLMWindowLayer, XPLMWindowPositioningMode,
};

pub unsafe fn create_xplm_window() -> XPLMWindowID {
    unsafe extern "C-unwind" fn draw_cb(win: XPLMWindowID, _: *mut c_void) {
        let Ok(mut g) = crate::plugin_cell().lock() else { return };
        if let Some(ps) = g.as_mut() {
            ps.gui.draw(win);
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
            ps.gui.on_mouse(win, x, y, s);
        }
        1
    }
    unsafe extern "C-unwind" fn cursor_cb(
        _win: XPLMWindowID,
        x: c_int,
        y: c_int,
        _: *mut c_void,
    ) -> xplane_sys::XPLMCursorStatus {
        let Ok(mut g) = crate::plugin_cell().lock() else {
            return xplane_sys::XPLMCursorStatus::Default;
        };
        if let Some(ps) = g.as_mut() {
            ps.gui.on_mouse_move(x, y);
        }
        xplane_sys::XPLMCursorStatus::Default
    }
    unsafe extern "C-unwind" fn wheel_cb(
        _win: XPLMWindowID,
        x: c_int,
        y: c_int,
        wheel: c_int,
        clicks: c_int,
        _: *mut c_void,
    ) -> c_int {
        let Ok(mut g) = crate::plugin_cell().lock() else { return 1 };
        if let Some(ps) = g.as_mut() {
            ps.gui.on_wheel(x, y, wheel, clicks);
        }
        1
    }
    unsafe extern "C-unwind" fn key_cb(
        _: XPLMWindowID,
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
                ps.gui.on_char(key as u8);
            }
        }
    }

    let mut params = xplane_sys::XPLMCreateWindow_t {
        structSize: std::mem::size_of::<xplane_sys::XPLMCreateWindow_t>() as c_int,
        left: 60,
        top: 460,
        right: 480,
        bottom: 60,
        visible: 0,
        drawWindowFunc: Some(draw_cb),
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
    XPLMSetWindowTitle(win, c"MumbledFlight".as_ptr());
    XPLMSetWindowPositioningMode(win, XPLMWindowPositioningMode::PositionFree, -1);
    win
}
