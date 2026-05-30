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

//! The `Ctx` panel-renderer struct and its reusable primitive widgets.
//!
//! `Ctx` groups the shared draw-time state (`ui`, `fw`, and the XPLM window
//! rectangle) so panel and file-picker methods — defined across the sibling
//! modules — don't repeat those parameters on every call.

use std::borrow::Cow;

/// X-coordinate where the widget column begins (pixels from window left edge).
/// Must match the `width - LABEL_COL_X` subtraction in `GuiState::draw`.
pub(super) const LABEL_COL_X: f32 = 115.0;

pub(super) struct Ctx<'ui> {
    pub(super) ui: &'ui imgui::Ui,
    pub(super) fw: f32,
    /// XPLM window top-left in ImGui virtual coordinates.
    pub(super) win_x: f32,
    pub(super) win_y: f32,
    pub(super) win_w: f32,
    pub(super) win_h: f32,
}

impl<'ui> Ctx<'ui> {
    // ── Primitive helpers ─────────────────────────────────────────────────────

    pub(super) fn row(&self, label: &str, id: &str, buf: &mut String) {
        self.ui.text(label);
        self.ui.same_line();
        self.ui.set_cursor_pos([LABEL_COL_X, self.ui.cursor_pos()[1]]);
        self.ui.set_next_item_width(self.fw);
        self.ui.input_text(id, buf).build();
    }

    /// Same layout as `row`, but the input is masked (for the server password).
    pub(super) fn password_row(&self, label: &str, id: &str, buf: &mut String) {
        self.ui.text(label);
        self.ui.same_line();
        self.ui.set_cursor_pos([LABEL_COL_X, self.ui.cursor_pos()[1]]);
        self.ui.set_next_item_width(self.fw);
        self.ui.input_text(id, buf).password(true).build();
    }

    /// Draws a semi-transparent overlay over the full XPLM window, sitting on top of all
    /// regular widgets but below the modal popup (which is a separate ImGui window).
    pub(super) fn draw_modal_dim(&self) {
        let dim = imgui::ImColor32::from_rgba_f32s(0.2, 0.2, 0.2, 0.35);
        unsafe {
            // Override the window's content-area scissor so the dim covers the full XPLM
            // window including its padding, not just the inner content region.
            imgui_sys::igPushClipRect(
                imgui_sys::ImVec2 {
                    x: self.win_x,
                    y: self.win_y,
                },
                imgui_sys::ImVec2 {
                    x: self.win_x + self.win_w,
                    y: self.win_y + self.win_h,
                },
                false,
            );
        }
        self.ui
            .get_window_draw_list()
            .add_rect(
                [self.win_x, self.win_y],
                [self.win_x + self.win_w, self.win_y + self.win_h],
                dim,
            )
            .filled(true)
            .build();
        unsafe { imgui_sys::igPopClipRect() }
    }

    /// Same layout as `row` but with a `…` browse button; returns `true` when clicked.
    pub(super) fn file_row(&self, label: &str, id: &str, buf: &mut String) -> bool {
        let style = self.ui.clone_style();
        let btn_w = self.ui.calc_text_size("...")[0] + style.frame_padding[0] * 2.0;
        let spacing = style.item_spacing[0];
        self.ui.text(label);
        self.ui.same_line();
        self.ui.set_cursor_pos([LABEL_COL_X, self.ui.cursor_pos()[1]]);
        self.ui.set_next_item_width(self.fw - btn_w - spacing);
        self.ui.input_text(id, buf).build();
        self.ui.same_line();
        self.ui.button(format!("...{id}_b"))
    }

    // A labelled slider plus a reset icon; the parameters map 1:1 to imgui's slider config.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn slider(
        &self,
        label: &str,
        id: &str,
        v: &mut f32,
        min: f32,
        max: f32,
        flags: imgui::SliderFlags,
        default: f32,
    ) {
        let icon_sz = self.ui.current_font_size();
        let spacing = self.ui.clone_style().item_spacing[0];

        self.ui.text(label);
        self.ui.same_line();
        self.ui.set_cursor_pos([LABEL_COL_X, self.ui.cursor_pos()[1]]);
        self.ui.set_next_item_width(self.fw - icon_sz - spacing);
        self.ui
            .slider_config(id, min, max)
            .flags(flags)
            .display_format("")
            .build(v);
        self.ui.same_line();
        // id is "##foo"; "_r" suffix gives the button a distinct imgui ID ("foo_r" vs "foo").
        if self.reset_icon_button(&format!("{id}_r")) {
            *v = default;
        }
    }

    /// Draws a circular-arrow icon button and returns `true` when clicked.
    /// Occupies `current_font_size` × `frame_height` so the icon sits centred
    /// against any adjacent widget with standard frame padding.
    fn reset_icon_button(&self, id: &str) -> bool {
        let icon_sz = self.ui.current_font_size();
        let frame_h = self.ui.frame_height();

        let clicked = self.ui.invisible_button(id, [icon_sz, frame_h]);
        let hovered = self.ui.is_item_hovered();
        let rect_min = self.ui.item_rect_min();
        let rect_max = self.ui.item_rect_max();

        // Centre derived from the actual placed rect — immune to spacing offsets.
        let cx = (rect_min[0] + rect_max[0]) * 0.5;
        let cy = (rect_min[1] + rect_max[1]) * 0.5;
        let r = icon_sz * 0.28;

        let col: [f32; 4] = if hovered {
            [1.0, 1.0, 1.0, 1.0]
        } else {
            [0.55, 0.55, 0.55, 1.0]
        };

        // Arc: ~306° clockwise (increasing θ = clockwise in screen/y-down coords).
        // Starts at ~36° (lower-right), ends at ~342° (upper-right); gap on the right side.
        use std::f32::consts::PI;
        let start_a = PI * 0.2;
        let sweep = PI * 1.7;
        let end_a = start_a + sweep;
        const N: usize = 14;
        let arc: Vec<[f32; 2]> = (0..=N)
            .map(|i| {
                let a = start_a + sweep * i as f32 / N as f32;
                [cx + r * a.cos(), cy + r * a.sin()]
            })
            .collect();
        let draw = self.ui.get_window_draw_list();
        draw.add_polyline(arc, col).thickness(1.5).build();

        // Filled arrowhead at arc end pointing in the clockwise tangent direction.
        // Clockwise tangent at θ in screen (y-down) coords: (−sin θ, cos θ).
        let tip = [cx + r * end_a.cos(), cy + r * end_a.sin()];
        let (tx, ty) = (-end_a.sin(), end_a.cos()); // clockwise tangent
        let (nx, ny) = (end_a.cos(), end_a.sin()); // outward radial normal
        let al = r * 0.55;
        let aw = r * 0.40;
        let p2 = [tip[0] - al * tx + aw * nx, tip[1] - al * ty + aw * ny];
        let p3 = [tip[0] - al * tx - aw * nx, tip[1] - al * ty - aw * ny];
        draw.add_triangle(tip, p2, p3, col).filled(true).build();

        if hovered {
            self.ui.tooltip_text("Reset");
        }
        clicked
    }

    pub(super) fn combo(&self, label: &str, id: &str, labels: &[String], selected: &mut i32) {
        let preview = labels
            .get(*selected as usize)
            .map(|s| s.as_str())
            .unwrap_or("(default)");
        self.ui.text(label);
        self.ui.same_line();
        self.ui.set_cursor_pos([LABEL_COL_X, self.ui.cursor_pos()[1]]);
        self.ui.set_next_item_width(self.fw);
        if let Some(_tok) = self.ui.begin_combo(id, preview) {
            let avail_w = self.ui.content_region_avail()[0];
            for (i, lbl) in labels.iter().enumerate() {
                let display = fit_label(self.ui, lbl, avail_w);
                if self
                    .ui
                    .selectable_config(&*display)
                    .selected(*selected == i as i32)
                    .build()
                {
                    *selected = i as i32;
                }
            }
        }
    }
}

/// Truncate `text` with `...` so it fits within `max_px` using ImGui's font metrics.
fn fit_label<'a>(ui: &imgui::Ui, text: &'a str, max_px: f32) -> Cow<'a, str> {
    if ui.calc_text_size(text)[0] <= max_px {
        return Cow::Borrowed(text);
    }
    let ell_w = ui.calc_text_size("...")[0];
    let avail = (max_px - ell_w).max(0.0);
    let mut end = text.len();
    while end > 0 {
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        if ui.calc_text_size(&text[..end])[0] <= avail {
            break;
        }
        end -= 1;
    }
    Cow::Owned(format!("{}...", &text[..end]))
}
