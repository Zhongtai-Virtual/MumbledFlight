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

//! The modal file-browser popup (`##fp`) used to pick the client certificate
//! and server CA files.

use std::path::PathBuf;

use imgui::MouseButton;

use super::super::{FilePickTarget, FilePicker};
use super::widgets::Ctx;

pub(super) enum FilePick {
    Open,
    Closed,
    Selected(FilePickTarget, String),
}

impl<'ui> Ctx<'ui> {
    /// Renders the `##fp` modal popup driven by `picker`.
    ///
    /// Returns `FilePick::Closed` when the popup is not visible (already dismissed),
    /// `FilePick::Open` while the user is browsing, and `FilePick::Selected` when a
    /// file is confirmed.
    pub(super) fn file_picker_modal(&self, picker: &mut FilePicker) -> FilePick {
        // Resolve the picker's centre position in screen space.
        // On the first frame win_offset is None → start centred; after that we read back
        // ImGui's own window position so user drags are preserved even if the XPLM window moves.
        let win_cx = self.win_x + self.win_w * 0.5;
        let win_cy = self.win_y + self.win_h * 0.5;
        let [off_x, off_y] = picker.win_offset.unwrap_or([0.0, 0.0]);
        unsafe {
            imgui_sys::igSetNextWindowPos(
                imgui_sys::ImVec2 {
                    x: win_cx + off_x,
                    y: win_cy + off_y,
                },
                imgui::Condition::Always as i32,
                imgui_sys::ImVec2 { x: 0.5, y: 0.5 },
            );
            if picker.win_offset.is_none() {
                imgui_sys::igSetNextWindowSize(
                    imgui_sys::ImVec2 { x: 430.0, y: 330.0 },
                    imgui::Condition::Always as i32,
                );
            }
            // Min: enough vertical room for path bar + ~6 list rows + button row.
            // Max: never exceed the XPLM window, which is the boundary for mouse-event delivery.
            let row_h = self.ui.frame_height_with_spacing();
            let min_w = self.ui.calc_text_size("Cancel  Select  ^ Up")[0]
                + self.ui.clone_style().frame_padding[0] * 6.0;
            imgui_sys::igSetNextWindowSizeConstraints(
                imgui_sys::ImVec2 { x: min_w, y: row_h * 8.0 },
                imgui_sys::ImVec2 { x: self.win_w, y: self.win_h },
                None,
                std::ptr::null_mut(),
            );
        }
        let Some(_token) = self
            .ui
            .modal_popup_config("##fp")
            .movable(true)
            .begin_popup()
        else {
            return FilePick::Closed;
        };

        // Record where the picker ended up so we can re-anchor it next frame.
        // Skip the readback on the very first frame: window_pos() returns stale values before
        // ImGui has committed the initial layout, which would corrupt the offset immediately.
        if picker.win_offset.is_some() {
            let [px, py] = self.ui.window_pos();
            let [pw, ph] = self.ui.window_size();
            picker.win_offset = Some([px + pw * 0.5 - win_cx, py + ph * 0.5 - win_cy]);
        } else {
            picker.win_offset = Some([0.0, 0.0]);
        }

        // ── Path bar ────────────────────────────────────────────────────────
        let path_str = picker.current_dir.display().to_string();
        let start = {
            let s = path_str.len().saturating_sub(48);
            (s..=path_str.len())
                .find(|&i| path_str.is_char_boundary(i))
                .unwrap_or(0)
        };
        self.ui.text(&path_str[start..]);

        // ── Entry list ───────────────────────────────────────────────────────
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
        let up_w = self.ui.calc_text_size("^ Up")[0] + self.ui.clone_style().frame_padding[0] * 2.0;
        let right_x = self.ui.window_content_region_max()[0] - up_w;
        self.ui.same_line_with_pos(right_x);
        let go_up = self.ui.button("^ Up##fpup");

        // Apply navigation or selection from the list.
        if go_up {
            picker.up();
        } else if let Some(dir) = navigate_to {
            picker.current_dir = dir;
            picker.refresh();
        } else {
            picker.selected = new_selected;
        }

        if cancel {
            self.ui.close_current_popup();
            return FilePick::Open; // Closed is returned next frame once ImGui tears it down.
        }

        let path = double_click_path.or_else(|| {
            select
                .then(|| picker.selected_path().map(|p| p.display().to_string()))
                .flatten()
        });

        if let Some(path) = path {
            self.ui.close_current_popup();
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
