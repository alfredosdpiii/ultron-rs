//! Mouse layout and hit-testing dispatch for [`super::App`].

use super::*;

/// The mouse-aware parts of a fullscreen frame (`currentLayout`): the dock slot holding the editor or the selector
/// in its place, and the transcript with the rows of each item.
#[derive(Clone, Debug, Default)]
pub(super) struct MouseLayout {
    pub(super) slot: Option<tui::Rect>,
    pub(super) transcript: Option<tui::Rect>,
    /// Header rows above the first transcript item.
    pub(super) header_rows: usize,
    pub(super) item_heights: Vec<usize>,
}

/// The dock slot's [`tui::MouseTarget`] id; transcript item `i` is `i + 1`.
const SLOT_TARGET: usize = 0;

/// The app's components for [`tui::FullscreenViewport::handle_input_with`]: they change themselves;
/// what the app does about a selection or a settings change is collected and done after the gesture.
pub(super) struct AppMouse<'a> {
    pub(super) layout: &'a MouseLayout,
    pub(super) theme: &'a Rc<Theme>,
    pub(super) now: Instant,
    pub(super) rows: usize,
    /// The transcript's first shown row.
    pub(super) top: usize,
    pub(super) editor: &'a mut Editor,
    pub(super) chat: &'a mut ChatView,
    pub(super) selection: &'a mut Option<Selection>,
    pub(super) panel: &'a mut Option<Panel>,
    pub(super) select_events: Vec<SelectEvent>,
    pub(super) settings_events: Vec<SettingsEvent>,
}

impl AppMouse<'_> {
    /// The dock slot's component (`event` in the slot's rows).
    fn slot_mouse(&mut self, event: &tui::MouseEvent) -> Option<tui::MouseResult> {
        if let Some(selection) = self.selection.as_mut() {
            let title = Text::new(&bold(&selection.title), 1, 1, None).render(event.width).len() as i64;
            let local = tui::MouseEvent {
                y: event.y - title,
                ..*event
            };
            let mut events = Vec::new();
            let result = selection.list.handle_mouse(&local, &mut events);
            self.select_events.extend(events);
            return result;
        }
        match self.panel.as_mut() {
            None => self.editor.handle_mouse(event),
            Some(Panel::Settings(selector)) => {
                let (result, ev) = selector.handle_mouse(event)?;
                self.settings_events.extend(ev);
                Some(result)
            }
            Some(panel) => panel_mouse(panel, self.theme, self.now, self.rows, event),
        }
    }

    fn dispatch(&mut self, target: tui::MouseTarget, event: &tui::MouseEvent) -> Option<tui::MouseDispatch> {
        let result = if target.id == SLOT_TARGET {
            self.slot_mouse(event)?
        } else {
            self.chat.handle_item_mouse(target.id - 1, event)?
        };
        Some(tui::MouseDispatch { result, target })
    }
}

fn panel_mouse(
    panel: &mut Panel,
    theme: &Rc<Theme>,
    now: Instant,
    rows: usize,
    event: &tui::MouseEvent,
) -> Option<tui::MouseResult> {
    match panel {
        Panel::OAuthLogin(dialog, _) => dialog.handle_mouse(theme, event),
        Panel::LoginPrompt {
            provider,
            method,
            input,
        } => login_prompt_mouse(theme, &provider.name, method, input, event),
        Panel::ExtensionInput { dialog, .. } => dialog.handle_mouse(event, now),
        Panel::TextEditor { dialog, .. } => dialog.handle_mouse(event, rows),
        Panel::Sessions(selector) | Panel::PiSessions(selector) => selector.handle_mouse(event),
        Panel::Login(selector, ..) | Panel::Logout(selector) => selector.handle_mouse(event),
        Panel::ScopedModels { selector, .. } => selector.handle_mouse(event),
        Panel::BugReport(flow) => flow.handle_mouse(event, rows),
        _ => None,
    }
}

fn login_prompt_mouse(
    theme: &Theme,
    provider_name: &str,
    method: &str,
    input: &mut tui::Input,
    event: &tui::MouseEvent,
) -> Option<tui::MouseResult> {
    let text = |s: &str| Text::new(s, 1, 0, None).render(event.width).len() as i64;
    let title = text(&theme.fg(Fg::Accent, &bold(&format!("Login to {provider_name}"))));
    let top = 1 + title + 1 + text(&theme.fg(Fg::Text, &format!("Enter {method}")));
    let rows = input.render(event.width).len() as i64;
    if event.y < top || event.y >= top + rows {
        return None;
    }
    input.handle_mouse(&tui::MouseEvent {
        y: event.y - top,
        ..*event
    })
}

impl tui::MouseTargets for AppMouse<'_> {
    fn dispatch_at(&mut self, event: &tui::MouseEvent) -> Option<tui::MouseDispatch> {
        use tui::{MouseTarget, retarget_mouse_event};
        let (x, y) = (event.screen_x, event.screen_y);
        if let Some(rect) = self.layout.slot.filter(|r| r.contains(x, y)) {
            let target = MouseTarget {
                id: SLOT_TARGET,
                origin_x: rect.x as i64,
                origin_y: rect.y as i64,
                width: rect.width,
                height: rect.height,
            };
            return self.dispatch(target, &retarget_mouse_event(event, &target));
        }
        let rect = self.layout.transcript.filter(|r| r.contains(x, y))?;
        let row = (self.top as i64 + y - rect.y as i64).checked_sub(self.layout.header_rows as i64)?;
        let mut start = 0i64;
        for (i, &height) in self.layout.item_heights.iter().enumerate() {
            let end = start + height as i64;
            if row >= start && row < end {
                let target = MouseTarget {
                    id: i + 1,
                    origin_x: rect.x as i64,
                    origin_y: rect.y as i64 + self.layout.header_rows as i64 + start - self.top as i64,
                    width: rect.width,
                    height,
                };
                return self.dispatch(target, &retarget_mouse_event(event, &target));
            }
            start = end;
        }
        None
    }

    fn dispatch_to(&mut self, target: &tui::MouseTarget, event: &tui::MouseEvent) -> Option<tui::MouseDispatch> {
        self.dispatch(*target, event)
    }
}
