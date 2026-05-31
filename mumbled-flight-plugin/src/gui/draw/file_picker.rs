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

//! File-browser content and XPLM window draw lifecycle for the file-picker popup.

use std::path::PathBuf;
use std::time::Instant;

use imgui::MouseButton;
use xplane_sys::{XPLMSetGraphicsState, XPLMSetWindowIsVisible, XPLMWindowID};

use super::{init_imgui, window_metrics};
use super::super::{FilePickTarget, FilePicker, GuiState};
use super::widgets::{Ctx, LABEL_COL_X};

pub(super) enum FilePick {
    Open,
    Closed,
    Selected(FilePickTarget, String),
}

impl<'ui> Ctx<'ui> {
    /// Renders the file-picker UI directly into the current ImGui window.
    ///
    /// Returns `FilePick::Closed` when the user clicks Cancel, `FilePick::Open`
    /// while browsing, and `FilePick::Selected` when a file is confirmed.
    pub(super) fn file_picker_content(&self, picker: &mut FilePicker) -> FilePick {
        // ── Path bar ─────────────────────────────────────────────────────────
        let path_str = picker.current_dir.display().to_string();
        let start = {
            let s = path_str.len().saturating_sub(48);
            (s..=path_str.len())
                .find(|&i| path_str.is_char_boundary(i))
                .unwrap_or(0)
        };
        self.ui.text(&path_str[start..]);

        // ── Entry list ────────────────────────────────────────────────────────
        let mut navigate_to: Option<PathBuf> = None;
        let mut new_selected = picker.selected;
        let mut double_click_path: Option<String> = None;

        self.ui
            .child_window("##fplist")
            .size([0.0, -self.ui.frame_height_with_spacing()])
            .border(true)
            .build(|| {
                for (i, entry) in picker.entries.iter().enumerate() {
                    let display = if entry.is_dir {
                        format!("[D] {}", entry.name)
                    } else {
                        format!("    {}", entry.name)
                    };
                    let is_sel = new_selected == Some(i);
                    if self
                        .ui
                        .selectable_config(&format!("{display}##fpe{i}"))
                        .selected(is_sel)
                        .build()
                    {
                        if entry.is_dir {
                            navigate_to = Some(picker.current_dir.join(&entry.name));
                        } else {
                            new_selected = Some(i);
                        }
                    }
                    if self.ui.is_item_hovered()
                        && self.ui.is_mouse_double_clicked(MouseButton::Left)
                    {
                        if entry.is_dir {
                            navigate_to = Some(picker.current_dir.join(&entry.name));
                        } else {
                            double_click_path =
                                Some(picker.current_dir.join(&entry.name).display().to_string());
                        }
                    }
                }
            });

        // ── Buttons ──────────────────────────────────────────────────────────
        let cancel = self.ui.button("Cancel##fpcancel");
        self.ui.same_line();
        let can_select = picker.selected_path().is_some() || double_click_path.is_some();
        let _dis = self.ui.begin_disabled(!can_select);
        let select = self.ui.button("Select##fpselect");
        drop(_dis);
        let up_w =
            self.ui.calc_text_size("^ Up")[0] + self.ui.clone_style().frame_padding[0] * 2.0;
        let right_x = self.ui.window_content_region_max()[0] - up_w;
        self.ui.same_line_with_pos(right_x);
        let go_up = self.ui.button("^ Up##fpup");

        if go_up {
            picker.up();
        } else if let Some(dir) = navigate_to {
            picker.current_dir = dir;
            picker.refresh();
        } else {
            picker.selected = new_selected;
        }

        if cancel {
            return FilePick::Closed;
        }

        let path = double_click_path.or_else(|| {
            select
                .then(|| picker.selected_path().map(|p| p.display().to_string()))
                .flatten()
        });

        if let Some(path) = path {
            return FilePick::Selected(picker.target, path);
        }

        FilePick::Open
    }
}

/// Returns the starting directory for the file picker: the parent of the current
/// field value when it exists on disk, otherwise the plugin folder.
pub(super) fn start_dir(current: &str, plugin_dir: &std::path::Path) -> PathBuf {
    if let Some(dir) = PathBuf::from(current).parent().filter(|d| d.is_dir()) {
        dir.to_path_buf()
    } else {
        plugin_dir.to_path_buf()
    }
}

// ── XPLM window draw lifecycle ────────────────────────────────────────────────

impl GuiState {
    pub fn draw_file_picker(&mut self, win: XPLMWindowID) {
        if self.file_picker.is_none() {
            unsafe { XPLMSetWindowIsVisible(win, 0) };
            return;
        }
        if self.fp_imgui.ctx.is_none() {
            init_imgui(&mut self.fp_imgui);
        }

        let (width, height, virt_w, virt_h, scale_x, scale_y, win_imgui_x, win_imgui_y) =
            window_metrics(win);
        self.screen_h = virt_h;

        let dt = {
            let now = Instant::now();
            let d = (now - self.fp_imgui.last_time).as_secs_f32().max(1e-6);
            self.fp_imgui.last_time = now;
            d
        };

        let mut file_picker = self.file_picker.take();
        let mut cert_path = self.cert_path.clone();
        let mut server_ca = self.server_ca.clone();
        let file_picker_win = self.file_picker_win;
        let mouse_pos = self.fp_imgui.mouse_pos;
        let mouse_down = self.fp_imgui.mouse_down;

        let (Some(ctx), Some(renderer)) =
            (self.fp_imgui.ctx.as_mut(), self.fp_imgui.renderer.as_mut())
        else {
            self.file_picker = file_picker;
            return;
        };

        {
            let io = ctx.io_mut();
            io.display_size = [virt_w as f32, virt_h as f32];
            io.display_framebuffer_scale = [scale_x, scale_y];
            io.delta_time = dt;
            io.mouse_pos = mouse_pos;
            io.mouse_down = mouse_down;
        }

        let mut close = false;
        {
            let ui = ctx.frame();
            let pad_r = ui.clone_style().window_padding[0];
            let fw = (width as f32 - LABEL_COL_X - pad_r).max(80.0);
            let p = Ctx { ui: &*ui, fw };
            ui.window("##fp")
                .position([win_imgui_x, win_imgui_y], imgui::Condition::Always)
                .size([width as f32, height as f32], imgui::Condition::Always)
                .title_bar(false)
                .resizable(false)
                .movable(false)
                .build(|| {
                    if let Some(fp) = file_picker.as_mut() {
                        match p.file_picker_content(fp) {
                            FilePick::Open => {}
                            FilePick::Closed => {
                                file_picker = None;
                                close = true;
                            }
                            FilePick::Selected(target, path) => {
                                match target {
                                    FilePickTarget::UserCert => cert_path = path,
                                    FilePickTarget::ServerCa => server_ca = path,
                                }
                                file_picker = None;
                                close = true;
                            }
                        }
                    } else {
                        close = true;
                    }
                });
        }
        let draw_data = ctx.render();
        unsafe { XPLMSetGraphicsState(0, 1, 0, 0, 1, 0, 0) };
        renderer.render(draw_data).ok();

        self.file_picker = file_picker;
        self.cert_path = cert_path;
        self.server_ca = server_ca;
        if close {
            unsafe { XPLMSetWindowIsVisible(file_picker_win, 0) };
        }
    }
}
