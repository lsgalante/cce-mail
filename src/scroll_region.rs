//! App-owned scroll region replacing the dissolved `List` embedded base (the Phase 6q
//! ScrollRegion, ported from cce-system-settings). The email list was a pure scroll
//! frame (`List` with `columns: None`, whose visuals were just its internal ScrollBox):
//! the rows are drawn by the app at `get_item_draw_y` positions, so the widget
//! contributed only the flat background, the scrollbar, the scroll/virtualization math,
//! and wheel/drag input. `push_quads` reproduces `ScrollBox::extra_quads` verbatim
//! (flat `list_bg_color` fill + track + thumb — no border, no rounding).

use cce_ui::widget::{ElementState, Key, KeyEvent, MouseScrollDelta, NamedKey};

#[derive(Debug, Clone)]
pub struct ScrollRegion {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Row height, with `List::new`'s silent adjustment to `max(item_height, list_font + 14)`.
    pub item_height: f32,
    pub item_gap: f32,
    pub scroll_y: f32,
    pub content_h: f32,
    pub viewport_y: f32,
    pub viewport_h: f32,
    pub dragging: bool,
    drag_offset_y: f32,
    pub hovered: bool,
    /// Local stand-in for the legacy global focus flag (`ScrollBox::focus()` on any press
    /// inside the frame): set on a press that hits the region, cleared on one that misses.
    pub focused: bool,
}

impl ScrollRegion {
    pub fn new(item_height: f32, item_gap: f32) -> Self {
        let (_, font_size) = cce_ui::layout::list_font_parsed();
        Self {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            item_height: item_height.max(font_size + 14.0),
            item_gap,
            scroll_y: 0.0,
            content_h: 0.0,
            viewport_y: 0.0,
            viewport_h: 0.0,
            dragging: false,
            drag_offset_y: 0.0,
            hovered: false,
            focused: false,
        }
    }

    pub fn set_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.x = x;
        self.y = y;
        self.w = w;
        self.h = h;
    }

    /// The `List::update_bounds` count math: `content_h = count * (item_height + gap) + 4`.
    pub fn update_bounds(&mut self, count: usize, viewport_y: f32, viewport_h: f32) {
        self.content_h = count as f32 * (self.item_height + self.item_gap) + 4.0;
        self.viewport_y = viewport_y;
        self.viewport_h = viewport_h;
        self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll());
    }

    pub fn set_scroll_y(&mut self, val: f32) {
        self.scroll_y = val;
    }

    fn max_scroll(&self) -> f32 {
        (self.content_h - self.viewport_h).max(0.0)
    }

    pub fn hit(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    /// Row virtualization (`List::get_item_draw_y`): screen y for row `idx`, or `None`
    /// when the row isn't fully inside the viewport.
    pub fn get_item_draw_y(&self, idx: usize, offset: f32) -> Option<f32> {
        let virtual_y = idx as f32 * (self.item_height + self.item_gap) + offset;
        let draw_y = self.viewport_y + virtual_y - self.scroll_y;
        if draw_y >= self.viewport_y - 1.0
            && draw_y + self.item_height <= self.viewport_y + self.viewport_h + 1.0
        {
            Some(draw_y)
        } else {
            None
        }
    }

    /// Scrollbar geometry (`ScrollBox::extra_quads`): (sb_x, track_y, sb_w, track_h, thumb_y, thumb_h).
    fn scrollbar_geom(&self) -> (f32, f32, f32, f32, f32, f32) {
        let sb_w = cce_ui::layout::scrollbar_width();
        let sb_x = self.x + self.w - sb_w - 4.0;
        let track_h = self.viewport_h - 8.0;
        let track_y = self.viewport_y + 4.0;
        let visible_ratio = self.viewport_h / self.content_h.max(1.0);
        let thumb_h = if track_h <= 20.0 {
            track_h
        } else {
            (track_h * visible_ratio).clamp(20.0, track_h)
        };
        let scroll_ratio = if self.max_scroll() > 0.0 { self.scroll_y / self.max_scroll() } else { 0.0 };
        let thumb_y = track_y + scroll_ratio * (track_h - thumb_h);
        (sb_x, track_y, sb_w, track_h, thumb_y, thumb_h)
    }

    fn hit_scrollbar(&self, px: f32, py: f32) -> bool {
        if self.content_h <= self.viewport_h {
            return false;
        }
        let (sb_x, track_y, sb_w, track_h, _, _) = self.scrollbar_geom();
        px >= sb_x - 4.0 && px <= sb_x + sb_w + 4.0 && py >= track_y && py <= track_y + track_h
    }

    /// Left press: scrollbar thumb grab or track jump (`ScrollBox::mouse_input`), plus the
    /// press-inside focus / press-outside unfocus bookkeeping. Returns true only when the
    /// scrollbar consumed the press — a press on the rows falls through to them.
    pub fn press(&mut self, px: f32, py: f32) -> bool {
        self.focused = self.hit(px, py);
        if !self.hit_scrollbar(px, py) {
            self.dragging = false;
            return false;
        }
        self.dragging = true;
        let (_, track_y, _, track_h, thumb_y, thumb_h) = self.scrollbar_geom();
        let click_offset = py - thumb_y;
        if click_offset >= 0.0 && click_offset <= thumb_h {
            self.drag_offset_y = click_offset;
        } else {
            self.drag_offset_y = thumb_h / 2.0;
            let target = py - self.drag_offset_y;
            let ratio = if track_h - thumb_h > 0.0 {
                ((target - track_y) / (track_h - thumb_h)).clamp(0.0, 1.0)
            } else {
                0.0
            };
            self.scroll_y = ratio * self.max_scroll();
        }
        true
    }

    /// Returns whether a thumb drag was in progress (the caller's redraw signal).
    pub fn release(&mut self) -> bool {
        std::mem::take(&mut self.dragging)
    }

    fn drag_move(&mut self, py: f32) -> bool {
        let (_, track_y, _, track_h, _, thumb_h) = self.scrollbar_geom();
        let target = py - self.drag_offset_y;
        let ratio = if track_h - thumb_h > 0.0 {
            ((target - track_y) / (track_h - thumb_h)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let old = self.scroll_y;
        self.scroll_y = ratio * self.max_scroll();
        (self.scroll_y - old).abs() > 0.01
    }

    /// Pointer-move bookkeeping: forwards to an active thumb drag (returns true so the host
    /// treats it as a high-priority drag override), else just tracks hover for the border
    /// tint and the keyboard scope.
    pub fn cursor_moved(&mut self, px: f32, py: f32) -> bool {
        self.hovered = self.hit(px, py);
        if self.dragging {
            self.drag_move(py);
            return true;
        }
        false
    }

    pub fn wheel(&mut self, delta: &MouseScrollDelta, px: f32, py: f32) -> bool {
        if !self.hit(px, py) {
            return false;
        }
        let dy = match delta {
            MouseScrollDelta::LineDelta(_, y) => -y * 24.0,
            MouseScrollDelta::PixelDelta(pos) => -pos.y as f32,
        };
        let old = self.scroll_y;
        self.scroll_y = (self.scroll_y + dy).clamp(0.0, self.max_scroll());
        (self.scroll_y - old).abs() > 0.01
    }

    /// Hover/focus-scoped keyboard scrolling (`ScrollBox::keyboard_input` reached the boxes
    /// when focused or hovered; the dissolved region keeps both via its local flags).
    pub fn keyboard(&mut self, event: &KeyEvent) -> bool {
        if (!self.hovered && !self.focused) || event.state != ElementState::Pressed {
            return false;
        }
        let max = self.max_scroll();
        let old = self.scroll_y;
        if event.ctrl {
            match &event.logical_key {
                Key::Character(c) if c == "n" || c == "N" => self.scroll_y = (self.scroll_y + 24.0).clamp(0.0, max),
                Key::Character(c) if c == "p" || c == "P" => self.scroll_y = (self.scroll_y - 24.0).clamp(0.0, max),
                _ => return false,
            }
        } else {
            match &event.logical_key {
                Key::Named(NamedKey::ArrowDown) => self.scroll_y = (self.scroll_y + 24.0).clamp(0.0, max),
                Key::Named(NamedKey::ArrowUp) => self.scroll_y = (self.scroll_y - 24.0).clamp(0.0, max),
                Key::Named(NamedKey::PageDown) => self.scroll_y = (self.scroll_y + self.viewport_h).clamp(0.0, max),
                Key::Named(NamedKey::PageUp) => self.scroll_y = (self.scroll_y - self.viewport_h).clamp(0.0, max),
                Key::Named(NamedKey::Home) => self.scroll_y = 0.0,
                Key::Named(NamedKey::End) => self.scroll_y = max,
                _ => return false,
            }
        }
        (self.scroll_y - old).abs() > 0.01
    }

    /// The legacy frame, single-drawn: 1px rounded border (focus/hover tinted, from
    /// `List::solid_border`), inset rounded bg, then the scrollbar track and thumb ON TOP.
    /// The legacy `ScrollBox::extra_quads` emission: flat background, then the
    /// scrollbar track + thumb when the content overflows.
    pub fn push_quads(&self, quads: &mut Vec<(f32, f32, f32, f32, [f32; 4])>) {
        quads.push((self.x, self.y, self.w, self.h, cce_ui::color::list_bg_color()));
        if self.content_h > self.viewport_h {
            let (sb_x, track_y, sb_w, track_h, thumb_y, thumb_h) = self.scrollbar_geom();
            quads.push((sb_x, track_y, sb_w, track_h, cce_ui::color::scrollbar_track_color()));
            quads.push((sb_x, thumb_y, sb_w, thumb_h, cce_ui::color::scrollbar_thumb_color()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region() -> ScrollRegion {
        // item_height clamps to list_font + 14, so pick one comfortably above any config.
        let mut r = ScrollRegion::new(40.0, 4.0);
        r.set_rect(10.0, 20.0, 200.0, 100.0);
        r
    }

    #[test]
    fn wheel_scrolls_and_clamps() {
        let mut r = region();
        r.update_bounds(10, 20.0, 100.0); // content_h = 444 > 100
        assert!(r.wheel(&MouseScrollDelta::LineDelta(0.0, -2.0), 50.0, 50.0));
        assert_eq!(r.scroll_y, 48.0);
        assert!(!r.wheel(&MouseScrollDelta::LineDelta(0.0, -2.0), 500.0, 50.0)); // miss
        r.wheel(&MouseScrollDelta::LineDelta(0.0, -100.0), 50.0, 50.0);
        assert_eq!(r.scroll_y, 344.0); // clamped to max_scroll
    }

    #[test]
    fn virtualization_matches_list_math() {
        let mut r = region();
        r.update_bounds(10, 20.0, 100.0);
        r.set_scroll_y(0.0);
        // Row 0 at viewport_y + 0*(44) + 4 = 24; fits (24 + 40 <= 121).
        assert_eq!(r.get_item_draw_y(0, 4.0), Some(24.0));
        // Row 2 at 20 + 92 - 0 = 112; 112 + 40 > 121 → culled.
        assert!(r.get_item_draw_y(2, 4.0).is_none());
    }

    #[test]
    fn press_focuses_and_grabs_only_scrollbar() {
        let mut r = region();
        r.update_bounds(10, 20.0, 100.0);
        // Press in the rows area: focused, not dragging, falls through.
        assert!(!r.press(50.0, 50.0));
        assert!(r.focused && !r.dragging);
        // Press on the scrollbar strip (x + w - sb_w - 4 ± 4): consumed.
        let sb_x = 10.0 + 200.0 - cce_ui::layout::scrollbar_width() - 4.0;
        assert!(r.press(sb_x + 1.0, 50.0));
        assert!(r.dragging);
        assert!(r.release());
        // Press outside: unfocuses.
        assert!(!r.press(500.0, 500.0));
        assert!(!r.focused);
    }

    #[test]
    fn keyboard_is_hover_or_focus_scoped() {
        let mut r = region();
        r.update_bounds(10, 20.0, 100.0);
        let down = KeyEvent {
            state: ElementState::Pressed,
            logical_key: Key::Named(NamedKey::ArrowDown),
            text: None,
            repeat: false,
            ctrl: false,
            shift: false,
        };
        assert!(!r.keyboard(&down)); // neither hovered nor focused
        r.cursor_moved(50.0, 50.0);
        assert!(r.hovered);
        assert!(r.keyboard(&down));
        assert_eq!(r.scroll_y, 24.0);
    }
}
