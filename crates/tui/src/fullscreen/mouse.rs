use std::time::Instant;

use crate::mouse::{MouseButton, MouseEvent, MouseEventType};
use crate::scroll_view::{ScrollbarGeometry, scrollbar_geometry};

use super::{
    DOUBLE_CLICK_INTERVAL, FullscreenViewport, Granularity, MouseDispatch, MouseTarget, MouseTargets, Sgr,
    ViewportInput, decode_button, is_right_click_press, links, retarget_mouse_event,
};

fn classify_sgr_kind(ev: Sgr) -> MouseEventType {
    if ev.release {
        MouseEventType::Release
    } else if ev.button & 32 != 0 {
        if decode_button(ev.button) == MouseButton::None {
            MouseEventType::Move
        } else {
            MouseEventType::Drag
        }
    } else {
        MouseEventType::Press
    }
}

impl FullscreenViewport {
    pub(super) fn handle_focus_out(&mut self) -> ViewportInput {
        let had_active = self.press_active;
        self.press_active = false;
        self.stop_auto_scroll();
        self.set_hover(false);
        self.pointer = None;
        self.scrollbar_drag = None;
        self.pressed_url = None;
        self.dragged = false;
        self.clear_component_gesture();
        self.last_component_click = None;
        if had_active {
            self.anchor = None;
            self.focus = None;
            self.granularity = Granularity::Character;
            self.initial = None;
        }
        self.last_click = None;
        ViewportInput::Consumed
    }

    pub(super) fn handle_wheel(
        &mut self,
        direction: i64,
        x: i64,
        y: i64,
        button: u32,
        targets: &mut dyn MouseTargets,
    ) -> ViewportInput {
        let lines = direction * self.wheel_scroll_lines(button);
        let mut event = self.mouse_event(MouseEventType::Wheel, button, x, y, None);
        event.wheel_delta = Some(lines);
        self.pointer = None;
        if let Some(d) = targets.dispatch_at(&event) {
            self.apply_dispatch(d);
            return ViewportInput::Consumed;
        }
        self.view.scroll_by(lines);
        self.update_hover(x, y);
        self.point_at(x, y);
        ViewportInput::Consumed
    }

    pub(super) fn wheel_scroll_lines(&self, button: u32) -> i64 {
        if button & 8 != 0 {
            self.wheel_lines * super::ALT_WHEEL_SCROLL_MULTIPLIER
        } else {
            self.wheel_lines
        }
    }

    /// `createMouseEvent`: screen coordinates over the whole terminal.
    pub(super) fn mouse_event(
        &self,
        kind: MouseEventType,
        button: u32,
        x: i64,
        y: i64,
        click_count: Option<u32>,
    ) -> MouseEvent {
        MouseEvent {
            kind,
            button: if kind == MouseEventType::Wheel {
                MouseButton::None
            } else {
                decode_button(button)
            },
            x,
            y,
            screen_x: x,
            screen_y: y,
            width: self.cols.max(1),
            height: self.rows.max(1),
            shift: button & 4 != 0,
            alt: button & 8 != 0,
            ctrl: button & 16 != 0,
            wheel_delta: None,
            click_count,
        }
    }

    /// `applyMouseDispatchResult`: a capture keeps the following events on the component.
    pub(super) fn apply_dispatch(&mut self, dispatch: MouseDispatch) {
        if dispatch.result.capture == Some(true) {
            self.mouse_capture = Some(dispatch.target);
        }
    }

    /// `clearComponentMouseGesture`.
    pub(super) fn clear_component_gesture(&mut self) {
        self.mouse_capture = None;
        self.press_target = None;
        self.press_point = None;
        self.press_moved = false;
    }

    fn component_click_count(&mut self, target: &MouseTarget, x: i64, y: i64, now: Instant) -> u32 {
        let count = match self.last_component_click {
            Some((at, count, id, px, py))
                if now.saturating_duration_since(at) <= DOUBLE_CLICK_INTERVAL
                    && id == target.id
                    && px == x
                    && py == y =>
            {
                count % 3 + 1
            }
            _ => 1,
        };
        self.last_component_click = Some((now, count, target.id, x, y));
        count
    }

    fn finish_captured_release(&mut self, target: MouseTarget, ev: Sgr, now: Instant, targets: &mut dyn MouseTargets) {
        if !self.press_moved && self.press_point == Some((ev.x, ev.y)) {
            let count = self.component_click_count(&target, ev.x, ev.y, now);
            let click = self.mouse_event(MouseEventType::Click, ev.button, ev.x, ev.y, Some(count));
            if let Some(d) = targets.dispatch_to(&target, &retarget_mouse_event(&click, &target)) {
                self.apply_dispatch(d);
            }
        }
        self.clear_component_gesture();
    }

    fn handle_captured_mouse(
        &mut self,
        target: MouseTarget,
        ev: Sgr,
        event: &MouseEvent,
        now: Instant,
        targets: &mut dyn MouseTargets,
    ) -> ViewportInput {
        if self.press_point.is_some_and(|p| p != (ev.x, ev.y)) {
            self.press_moved = true;
            self.last_component_click = None;
        }
        if let Some(d) = targets.dispatch_to(&target, &retarget_mouse_event(event, &target)) {
            self.apply_dispatch(d);
        }
        if ev.release {
            self.finish_captured_release(target, ev, now, targets);
        }
        ViewportInput::Consumed
    }

    fn handle_scrollbar_phase(&mut self, ev: Sgr) -> bool {
        let handled = self.scrollbar_mouse(ev);
        if self.scrollbar_drag.is_none() {
            self.update_hover(ev.x, ev.y);
        }
        handled
    }

    fn dispatch_target_press(&mut self, dispatch: MouseDispatch, kind: MouseEventType, ev: Sgr) {
        self.apply_dispatch(dispatch);
        if kind == MouseEventType::Press {
            self.clear_text_selection();
            self.press_target = Some(dispatch.target);
            self.press_point = Some((ev.x, ev.y));
            self.press_moved = false;
        }
    }

    /// `handleMouseEvent`: the component gesture in progress, the jump-to-end indicator, the scrollbar, the
    /// component under the pointer, right-click paste, then text selection. Hover is only a look, and the transcript
    /// items under the pointer take motion too, so a motion notes the pointer before they see it; a motion that moved
    /// neither hover is [`ViewportInput::Unchanged`]. A primary click on a link it would open skips those items:
    /// links take precedence over enclosing click regions.
    pub(super) fn handle_mouse(&mut self, ev: Sgr, now: Instant, targets: &mut dyn MouseTargets) -> ViewportInput {
        let kind = classify_sgr_kind(ev);
        let event = self.mouse_event(kind, ev.button, ev.x, ev.y, None);
        let shown = (self.scrollbar_hover, self.hovered_link());
        self.pointer = None;
        if let Some(target) = self.mouse_capture.or(self.press_target) {
            return self.handle_captured_mouse(target, ev, &event, now, targets);
        }
        if self.indicator_click(ev) || self.handle_scrollbar_phase(ev) {
            return ViewportInput::Consumed;
        }
        if kind == MouseEventType::Move {
            self.point_at(ev.x, ev.y);
        }
        if !self.primary_click_on_link(ev)
            && let Some(d) = targets.dispatch_at(&event)
        {
            self.dispatch_target_press(d, kind, ev);
            return ViewportInput::Consumed;
        }
        self.point_at(ev.x, ev.y);
        if kind == MouseEventType::Move && (self.scrollbar_hover, self.hovered_link()) == shown {
            return ViewportInput::Unchanged;
        }
        if self.right_click_paste && is_right_click_press(ev.button, ev.release) {
            return ViewportInput::RightClickPaste;
        }
        self.selection_mouse(ev, now, targets)
    }

    fn indicator_click(&mut self, ev: Sgr) -> bool {
        let Some((row, column, width)) = self.indicator_rect else {
            return false;
        };
        if ev.release || ev.button & 32 != 0 || ev.button & 3 != 0 {
            return false;
        }
        if ev.y != row as i64 || ev.x < column as i64 || ev.x >= (column + width) as i64 {
            return false;
        }
        self.view.scroll_to_end();
        true
    }

    pub(super) fn scrollbar_target(&self, x: i64, y: i64, include_hidden: bool) -> Option<ScrollbarGeometry> {
        let rect = self.rect?;
        if !rect.contains(x, y) {
            return None;
        }
        let g = scrollbar_geometry(&self.view, rect, self.content.len(), include_hidden)?;
        (x == g.column as i64 && y >= g.track_top as i64 && y < (g.track_top + g.track_height) as i64).then_some(g)
    }

    pub(super) fn set_hover(&mut self, hover: bool) {
        if hover == self.scrollbar_hover {
            return;
        }
        self.scrollbar_hover = hover;
        self.view.set_scrollbar_active(hover);
    }

    fn update_hover(&mut self, x: i64, y: i64) {
        let hover = self.scrollbar_target(x, y, true).is_some();
        self.set_hover(hover);
    }

    /// With link hover on, the pointer is at `x`, `y`; outside the transcript it is over no link.
    fn point_at(&mut self, x: i64, y: i64) {
        let inside = self.rect.is_some_and(|r| r.contains(x, y));
        self.pointer = (self.link_hover && inside).then_some((y as usize, x as usize));
    }

    /// A primary press or release over a link the click would open: an OSC 8 link, or with Option/Alt (SGR modifier
    /// bit 8) a plain-text URL.
    fn primary_click_on_link(&self, ev: Sgr) -> bool {
        let primary = ev.button & 64 == 0 && ev.button & 32 == 0 && (ev.button & 3 == 0 || ev.release);
        if !primary {
            return false;
        }
        let row = ev.y.clamp(0, (self.rows as i64 - 1).max(0)) as usize;
        let col = ev.x.clamp(0, (self.cols as i64 - 1).max(0)) as usize;
        self.screen
            .get(row)
            .and_then(|line| links::link_at(line, col, ev.button & 8 != 0))
            .is_some()
    }

    /// The link under the pointer in the last frame: its row and columns.
    fn hovered_link(&self) -> Option<(usize, usize, usize)> {
        let (row, col) = self.pointer?;
        let link = links::link_at(self.screen.get(row)?, col, true)?;
        Some((row, link.start, link.end))
    }

    fn scroll_to_pointer(&mut self, g: ScrollbarGeometry, pointer_y: i64, grab: i64) {
        let max_thumb = (g.track_height - g.thumb_height) as i64;
        let offset = (pointer_y - g.track_top as i64 - grab).clamp(0, max_thumb.max(0));
        let top = if max_thumb == 0 {
            0
        } else {
            (offset as f64 / max_thumb as f64 * g.max_scroll_top as f64 + 0.5).floor() as i64
        };
        self.view.scroll_to(top, false);
    }

    fn drag_scrollbar(&mut self, ev: Sgr, grab: i64) {
        if ev.release {
            self.scrollbar_drag = None;
            return;
        }
        if let Some(rect) = self.rect
            && let Some(g) = scrollbar_geometry(&self.view, rect, self.content.len(), false)
        {
            self.scroll_to_pointer(g, ev.y, grab);
        }
    }

    fn begin_scrollbar_drag(&mut self, ev: Sgr, g: ScrollbarGeometry) {
        self.stop_auto_scroll();
        self.press_active = false;
        self.anchor = None;
        self.focus = None;
        self.granularity = Granularity::Character;
        self.initial = None;
        self.last_click = None;
        self.pressed_url = None;
        self.dragged = false;
        self.set_hover(true);
        let on_thumb = ev.y >= g.thumb_top as i64 && ev.y < (g.thumb_top + g.thumb_height) as i64;
        let grab = if on_thumb {
            ev.y - g.thumb_top as i64
        } else {
            (g.thumb_height / 2) as i64
        };
        if !on_thumb {
            self.scroll_to_pointer(g, ev.y, grab);
        }
        self.scrollbar_drag = Some(grab);
    }

    fn scrollbar_mouse(&mut self, ev: Sgr) -> bool {
        if let Some(grab) = self.scrollbar_drag {
            self.drag_scrollbar(ev, grab);
            return true;
        }
        if ev.release || ev.button & 32 != 0 || ev.button & 3 != 0 {
            return false;
        }
        let Some(g) = self.scrollbar_target(ev.x, ev.y, false) else {
            return false;
        };
        self.begin_scrollbar_drag(ev, g);
        true
    }
}
