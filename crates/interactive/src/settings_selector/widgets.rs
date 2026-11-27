//! Reusable list and select-submenu widgets for [`super::SettingsSelector`].

use super::*;

/// `SettingsList`: aligned label/value rows, optional search, the selected item's description and a hint.
pub(super) struct List {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    pub(super) items: Vec<Item>,
    pub(super) filtered: Vec<usize>,
    pub(super) selected: usize,
    pub(super) max_visible: usize,
    pub(super) search: Option<Input>,
    /// The row a left press selected (`mousePressedIndex`).
    pressed: Option<usize>,
}

pub(super) enum ListAction {
    Changed(String, String),
    Open(usize),
    Cancel,
}

fn handled_mouse(render: Option<bool>, focus: Option<bool>) -> MouseResult {
    MouseResult {
        handled: Some(true),
        render,
        focus,
        ..MouseResult::default()
    }
}

fn handle_search_row_mouse(search: &mut Input, event: &MouseEvent) -> Option<(MouseResult, Option<ListAction>)> {
    if event.y != 0 {
        return None;
    }
    let result = search.handle_mouse(event)?;
    Some((
        MouseResult {
            focus: Some(true),
            ..result
        },
        None,
    ))
}

fn map_select_event(event: SelectEvent) -> SelectOutcome {
    match event {
        SelectEvent::Selected(item) => SelectOutcome::Picked(item.value),
        SelectEvent::Cancelled => SelectOutcome::Back,
        SelectEvent::SelectionChanged(item) => SelectOutcome::Highlight(item.value),
    }
}

impl List {
    pub(super) fn new(
        theme: Rc<Theme>,
        keys: KeybindingsManager,
        items: Vec<Item>,
        max_visible: usize,
        search: bool,
    ) -> Self {
        let filtered = (0..items.len()).collect();
        let search = search.then(|| {
            let mut input = Input::new(keys.clone());
            input.focused = true;
            input
        });
        List {
            theme,
            keys,
            items,
            filtered,
            selected: 0,
            max_visible,
            search,
            pressed: None,
        }
    }

    /// `SettingsList.handleMouse` (no submenu open): the search row goes to the search input, the wheel moves the
    /// selection, a left press selects the row and the click that follows activates the pressed row.
    pub(super) fn handle_mouse(&mut self, event: &MouseEvent) -> Option<(MouseResult, Option<ListAction>)> {
        if let Some(search) = self.search.as_mut()
            && matches!(event.y, 0 | 1)
        {
            return handle_search_row_mouse(search, event);
        }
        let n = self.filtered.len();
        if n == 0 {
            return None;
        }
        if event.kind == MouseEventType::Wheel {
            return self.handle_wheel_mouse(event.wheel_delta, n);
        }
        self.handle_click_mouse(event)
    }

    fn handle_wheel_mouse(
        &mut self,
        wheel_delta: Option<i64>,
        count: usize,
    ) -> Option<(MouseResult, Option<ListAction>)> {
        let delta = wheel_delta.filter(|d| *d != 0)?;
        let previous = self.selected;
        self.selected = if delta < 0 {
            self.selected.saturating_sub(1)
        } else {
            (self.selected + 1).min(count - 1)
        };
        Some((handled_mouse(Some(self.selected != previous), None), None))
    }

    fn row_index_at(&self, y: i64) -> Option<usize> {
        let offset = if self.search.is_some() { 2 } else { 0 };
        let (start, end) = self.range();
        let index = start as i64 + y - offset;
        (index >= start as i64 && index < end as i64).then_some(index as usize)
    }

    fn handle_click_mouse(&mut self, event: &MouseEvent) -> Option<(MouseResult, Option<ListAction>)> {
        if event.button != MouseButton::Left || !matches!(event.kind, MouseEventType::Press | MouseEventType::Click) {
            return None;
        }
        let index = self.row_index_at(event.y)?;
        if event.kind == MouseEventType::Press {
            self.pressed = Some(index);
            self.selected = index;
            return Some((handled_mouse(None, Some(true)), None));
        }
        self.selected = self.pressed.take().unwrap_or(index);
        Some((handled_mouse(None, None), self.activate()))
    }

    pub(super) fn update(&mut self, id: &str, value: &str) {
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.value = value.to_string();
        }
    }

    pub(super) fn range(&self) -> (usize, usize) {
        let n = self.filtered.len() as isize;
        let max = self.max_visible as isize;
        let start = (self.selected as isize - max / 2).min(n - max).max(0) as usize;
        (start, (start + self.max_visible).min(self.filtered.len()))
    }

    pub(super) fn hint(&self, lines: &mut Vec<String>, width: usize) {
        lines.push(String::new());
        let text = if self.search.is_some() {
            "  Type to search · Enter/Space to change · Esc to cancel"
        } else {
            "  Enter/Space to change · Esc to cancel"
        };
        lines.push(truncate_to_width(&self.theme.fg(Fg::Dim, text), width, "...", false));
    }

    pub(super) fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let mut lines = Vec::new();
        if let Some(search) = self.search.as_mut() {
            lines.extend(search.render(width));
            lines.push(String::new());
        }
        if self.items.is_empty() {
            lines.push(t.fg(Fg::Dim, "  No settings available"));
            if self.search.is_some() {
                self.hint(&mut lines, width);
            }
            return lines;
        }
        if self.filtered.is_empty() {
            lines.push(truncate_to_width(
                &t.fg(Fg::Dim, "  No matching settings"),
                width,
                "...",
                false,
            ));
            self.hint(&mut lines, width);
            return lines;
        }
        let (start, end) = self.range();
        let label_width = self
            .items
            .iter()
            .map(|i| visible_width(&i.label))
            .max()
            .unwrap_or(0)
            .min(36);
        for row in start..end {
            lines.push(self.render_row(row, label_width, width));
        }
        if start > 0 || end < self.filtered.len() {
            let text = format!("  ({}/{})", self.selected + 1, self.filtered.len());
            lines.push(t.fg(Fg::Dim, &truncate_to_width(&text, width.saturating_sub(2), "", false)));
        }
        self.render_selected_description(&mut lines, width);
        self.hint(&mut lines, width);
        lines
    }

    fn render_row(&self, row: usize, label_width: usize, width: usize) -> String {
        let t = &self.theme;
        let it = &self.items[self.filtered[row]];
        let selected = row == self.selected;
        let prefix = if selected {
            t.fg(Fg::Accent, "→ ")
        } else {
            "  ".to_string()
        };
        let padded = format!(
            "{}{}",
            it.label,
            " ".repeat(label_width.saturating_sub(visible_width(&it.label)))
        );
        let label = if selected { t.fg(Fg::Accent, &padded) } else { padded };
        let used = visible_width(&prefix) + label_width + 2;
        let value = truncate_to_width(&it.value, width.saturating_sub(used + 2), "", false);
        let value = if selected {
            t.fg(Fg::Accent, &value)
        } else {
            t.fg(Fg::Muted, &value)
        };
        truncate_to_width(&format!("{prefix}{label}  {value}"), width, "...", false)
    }

    fn render_selected_description(&self, lines: &mut Vec<String>, width: usize) {
        let Some(description) = self
            .filtered
            .get(self.selected)
            .and_then(|&i| self.items[i].description.as_deref())
        else {
            return;
        };
        lines.push(String::new());
        for line in wrap_text_with_ansi(description, width.saturating_sub(4)) {
            lines.push(self.theme.fg(Fg::Dim, &format!("  {line}")));
        }
    }

    pub(super) fn handle_input(&mut self, data: &str) -> Option<ListAction> {
        let k = |id: &str| self.keys.matches(data, id);
        let search_empty = self.search.as_ref().is_none_or(|s| s.value().is_empty());
        if k("tui.select.up") {
            self.select_prev();
        } else if k("tui.select.down") {
            self.select_next();
        } else if k("tui.select.confirm") || (data == " " && search_empty) {
            return self.activate();
        } else if k("tui.select.cancel") {
            return Some(ListAction::Cancel);
        } else {
            self.apply_search_input(data);
        }
        None
    }

    fn select_prev(&mut self) {
        let n = self.filtered.len();
        if n > 0 {
            self.selected = if self.selected == 0 { n - 1 } else { self.selected - 1 };
        }
    }

    fn select_next(&mut self) {
        let n = self.filtered.len();
        if n > 0 {
            self.selected = if self.selected + 1 >= n { 0 } else { self.selected + 1 };
        }
    }

    fn apply_search_input(&mut self, data: &str) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        search.handle_input(data);
        let query = search.value().to_string();
        let indexed: Vec<usize> = (0..self.items.len()).collect();
        self.filtered = fuzzy_filter(&indexed, &query, |&i| self.items[i].label.clone());
        self.selected = 0;
    }

    pub(super) fn activate(&mut self) -> Option<ListAction> {
        let &index = self.filtered.get(self.selected)?;
        let it = &mut self.items[index];
        if it.sub.is_some() {
            return Some(ListAction::Open(index));
        }
        if it.values.is_empty() {
            return None;
        }
        let at = it.values.iter().position(|v| *v == it.value);
        let next = it.values[at.map_or(0, |i| (i + 1) % it.values.len())].clone();
        it.value = next.clone();
        Some(ListAction::Changed(it.id.clone(), next))
    }
}

/// `SelectSubmenu`: a titled select list, optionally searchable.
pub(super) struct SelectSubmenu {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    pub(super) title: String,
    description: String,
    all: Vec<SelectItem>,
    pub(super) list: SelectList,
    pub(super) search: Option<Input>,
    layout: (usize, usize),
}

pub(super) enum SelectOutcome {
    Picked(String),
    Back,
    /// The highlighted item moved to this value.
    Highlight(String),
}

impl SelectSubmenu {
    pub(super) fn new(
        theme: Rc<Theme>,
        keys: KeybindingsManager,
        (title, description): (&str, &str),
        items: Vec<SelectItem>,
        current: &str,
        searchable: bool,
        layout: (usize, usize),
    ) -> Self {
        let list = Self::build(&theme, &items, current, layout);
        let search = searchable.then(|| {
            let mut input = Input::new(keys.clone());
            input.focused = true;
            input
        });
        SelectSubmenu {
            theme,
            keys,
            title: title.into(),
            description: description.into(),
            all: items,
            list,
            search,
            layout,
        }
    }

    pub(super) fn build(theme: &Rc<Theme>, items: &[SelectItem], current: &str, layout: (usize, usize)) -> SelectList {
        let mut list = SelectList::new(
            items.to_vec(),
            items.len().min(10),
            crate::components::select_theme(theme, Fg::Muted, Fg::Muted),
        );
        list.min_primary_column_width = Some(layout.0);
        list.max_primary_column_width = Some(layout.1);
        if let Some(i) = items.iter().position(|o| o.value == current) {
            list.set_selected_index(i);
        }
        list
    }

    pub(super) fn handle_input(&mut self, data: &str) -> Option<SelectOutcome> {
        let nav = [
            "tui.select.up",
            "tui.select.down",
            "tui.select.confirm",
            "tui.select.cancel",
        ]
        .iter()
        .any(|id| self.keys.matches(data, id));
        if let Some(search) = self.search.as_mut()
            && !nav
        {
            search.handle_input(data);
            let query = search.value().to_string();
            let filtered = if query.is_empty() {
                self.all.clone()
            } else {
                fuzzy_filter(&self.all, &query, |i| {
                    format!("{} {}", i.label, i.description.clone().unwrap_or_default())
                })
            };
            self.list = Self::build(&self.theme, &filtered, "", self.layout);
            return None;
        }
        self.list.handle_input(data).map(map_select_event)
    }

    /// `SelectSubmenu` as a `Container` (title, description, search input, list, hint): the search row goes to the
    /// input, the list rows to the `SelectList`; what the list answered, as [`Self::handle_input`] would.
    pub(super) fn handle_mouse(&mut self, event: &MouseEvent) -> Option<(MouseResult, Vec<SelectOutcome>)> {
        let width = event.width;
        let t = Rc::clone(&self.theme);
        let mut top = wrap_text_with_ansi(&bold(&t.fg(Fg::Accent, &self.title)), width).len() as i64;
        if !self.description.is_empty() {
            top += 1 + wrap_text_with_ansi(&t.fg(Fg::Muted, &self.description), width).len() as i64;
        }
        if let Some(search) = self.search.as_mut() {
            top += 1;
            let rows = search.render(width).len() as i64;
            if event.y >= top && event.y < top + rows {
                let local = MouseEvent {
                    y: event.y - top,
                    ..*event
                };
                return search.handle_mouse(&local).map(|r| (r, Vec::new()));
            }
            top += rows;
        }
        top += 1;
        let rows = self.list.render(width).len() as i64;
        if event.y < top || event.y >= top + rows {
            return None;
        }
        let local = MouseEvent {
            y: event.y - top,
            height: rows as usize,
            ..*event
        };
        let mut events = Vec::new();
        let result = self.list.handle_mouse(&local, &mut events)?;
        let outcomes = events.into_iter().map(map_select_event).collect();
        Some((result, outcomes))
    }

    pub(super) fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let mut lines = wrap_text_with_ansi(&bold(&t.fg(Fg::Accent, &self.title)), width);
        if !self.description.is_empty() {
            lines.push(String::new());
            lines.extend(wrap_text_with_ansi(&t.fg(Fg::Muted, &self.description), width));
        }
        if let Some(search) = self.search.as_mut() {
            lines.push(String::new());
            lines.extend(search.render(width));
        }
        lines.push(String::new());
        lines.extend(self.list.render(width));
        lines.push(String::new());
        let hint = if self.search.is_some() {
            "  Type to filter · Enter to select · Esc to go back"
        } else {
            "  Enter to select · Esc to go back"
        };
        lines.extend(wrap_text_with_ansi(&t.fg(Fg::Dim, hint), width));
        lines
    }
}
