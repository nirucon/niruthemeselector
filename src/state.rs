//! What the window shows and how input changes it, without X.
//!
//! The event loop in `window.rs` turns X events into calls here and runs the
//! `Command`s that come back (start an action on the worker, read the
//! clipboard, reload the catalogue, quit). Keeping this free of X makes the
//! scroll, filter, undo and import rules testable as plain functions.

use crate::{
    backend::{Action, Catalog, ThemeEntry},
    import::{self, PathStatus, Preview, Want},
    layout::{Button, Chip, Layout},
    theme::{self, Palette},
    thumbs,
};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// How long "Applied … Undo" stays.
pub const UNDO_FOR: Duration = Duration::from_secs(15);
/// Two clicks on one card within this many milliseconds apply it.
pub const DOUBLE_CLICK_MS: u32 = 400;

#[derive(Clone, Debug)]
pub struct Card {
    pub entry: ThemeEntry,
    pub palette: Palette,
    pub wallpaper: Option<PathBuf>,
    /// The wallpaper's preview stamp (`thumbs::key`), taken once per
    /// catalogue read rather than on every frame.
    pub stamp: String,
}

impl Card {
    pub fn origin(&self) -> &'static str {
        if self.entry.overridden {
            "Built-in override"
        } else if self.entry.builtin {
            "Built-in"
        } else {
            "User"
        }
    }

    /// "Built-in · hidden · invalid" style status.
    pub fn status(&self) -> String {
        let mut out = self.origin().to_string();
        if self.entry.hidden {
            out.push_str(" \u{b7} hidden");
        }
        if !self.entry.valid {
            out.push_str(" \u{b7} invalid");
        }
        out
    }

    fn matches(&self, chip: Chip, filter: &str) -> bool {
        let by_chip = match chip {
            Chip::All => !self.entry.hidden,
            Chip::Builtin => self.entry.builtin && !self.entry.hidden,
            Chip::User => (!self.entry.builtin || self.entry.overridden) && !self.entry.hidden,
            Chip::Hidden => self.entry.hidden,
        };
        if !by_chip {
            return false;
        }
        let filter = filter.trim().to_lowercase();
        filter.is_empty()
            || self.entry.name.to_lowercase().contains(&filter)
            || self.entry.id.to_lowercase().contains(&filter)
    }
}

/// One editable line with a cursor (in characters).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Field {
    pub text: String,
    pub cursor: usize,
}

impl Field {
    fn byte(&self, chars: usize) -> usize {
        self.text
            .char_indices()
            .nth(chars)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len())
    }

    pub fn insert(&mut self, text: &str) {
        let room = 1024usize.saturating_sub(self.text.chars().count());
        let text: String = text
            .chars()
            .filter(|c| !c.is_control())
            .take(room)
            .collect();
        let at = self.byte(self.cursor);
        self.text.insert_str(at, &text);
        self.cursor += text.chars().count();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let at = self.byte(self.cursor - 1);
        self.text.remove(at);
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.chars().count() {
            let at = self.byte(self.cursor);
            self.text.remove(at);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.chars().count());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }

    pub fn set(&mut self, text: String) {
        self.text = text;
        self.end();
    }
}

/// The three import fields.
pub const THEME_FIELD: usize = 0;
pub const WALLPAPER_FIELD: usize = 1;
pub const ID_FIELD: usize = 2;

#[derive(Clone, Debug, Default)]
pub struct ImportForm {
    pub fields: [Field; 3],
    pub focus: usize,
    pub replace: bool,
    pub theme_status: Option<PathStatus>,
    pub preview: Option<Result<Preview, String>>,
    pub wallpaper_status: Option<PathStatus>,
    pub error: Option<String>,
}

impl ImportForm {
    /// Check both paths again; called after every edit.
    pub fn validate(&mut self) {
        let (status, preview) = import::check_theme(&self.fields[THEME_FIELD].text);
        self.theme_status = Some(status);
        self.preview = preview;
        self.wallpaper_status = Some(import::check_wallpaper(&self.fields[WALLPAPER_FIELD].text));
    }

    /// The action this form would start, or why it cannot.
    pub fn action(&self) -> Result<Action, String> {
        let theme = self.fields[THEME_FIELD].text.trim();
        if theme.is_empty() {
            return Err("Enter the theme TOML path first.".into());
        }
        let (status, preview) = import::check_theme(theme);
        match status {
            PathStatus::Found => {}
            PathStatus::NotFound => return Err("The theme file was not found.".into()),
            other => return Err(format!("The theme file cannot be used: {}", other.label())),
        }
        if let Some(Err(error)) = preview {
            return Err(error);
        }
        let wallpaper = self.fields[WALLPAPER_FIELD].text.trim();
        match import::check_wallpaper(wallpaper) {
            PathStatus::Empty | PathStatus::Found => {}
            PathStatus::NotFound => return Err("The wallpaper was not found.".into()),
            PathStatus::Unusable(why) => return Err(format!("The wallpaper is {why}.")),
        }
        let id = self.fields[ID_FIELD].text.trim();
        if !id.is_empty()
            && !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err("A theme id uses letters, digits, - and _ only.".into());
        }
        Ok(Action::Import {
            theme: theme.into(),
            wallpaper: wallpaper.into(),
            id: id.into(),
            replace: self.replace,
        })
    }

    /// The id the import will create.
    pub fn imported_id(&self) -> Option<String> {
        let id = self.fields[ID_FIELD].text.trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
        Path::new(self.fields[THEME_FIELD].text.trim())
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
    }
}

/// The in-app directory list behind **Browse…**.
#[derive(Clone, Debug)]
pub struct Browse {
    pub form: ImportForm,
    pub want: Want,
    pub dir: PathBuf,
    pub entries: Vec<import::Entry>,
    pub selected: usize,
    pub scroll: usize,
    pub error: Option<String>,
}

impl Browse {
    pub fn open(form: ImportForm, want: Want, dir: PathBuf) -> Self {
        let mut browse = Self {
            form,
            want,
            dir: PathBuf::new(),
            entries: Vec::new(),
            selected: 0,
            scroll: 0,
            error: None,
        };
        browse.go(dir);
        browse
    }

    pub fn go(&mut self, dir: PathBuf) {
        match import::list_dir(&dir, self.want) {
            Ok(entries) => {
                self.entries = entries;
                self.dir = dir;
                self.error = None;
            }
            Err(error) => {
                if self.dir.as_os_str().is_empty() {
                    self.dir = dir;
                }
                self.error = Some(error);
            }
        }
        self.selected = 0;
        self.scroll = 0;
    }

    pub fn up(&mut self) {
        if let Some(parent) = self.dir.parent().map(Path::to_path_buf) {
            let from = self
                .dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string());
            self.go(parent);
            if let Some(from) = from {
                if let Some(index) = self.entries.iter().position(|e| e.name == from) {
                    self.selected = index;
                }
            }
        }
    }

    /// Open the selected folder, or choose the selected file: the form comes
    /// back with the path filled in.
    pub fn choose(mut self, index: usize) -> Result<ImportForm, Box<Self>> {
        let Some(entry) = self.entries.get(index).cloned() else {
            return Err(Box::new(self));
        };
        let path = self.dir.join(&entry.name);
        if entry.dir {
            self.go(path);
            return Err(Box::new(self));
        }
        let field = match self.want {
            Want::Theme => THEME_FIELD,
            Want::Wallpaper => WALLPAPER_FIELD,
        };
        let mut form = self.form;
        form.fields[field].set(import::display_path(&path, import::home().as_deref()));
        form.focus = field;
        form.validate();
        Ok(form)
    }

    pub fn keep_visible(&mut self, rows: usize) {
        let rows = rows.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + rows {
            self.scroll = self.selected + 1 - rows;
        }
        self.scroll = self.scroll.min(self.entries.len().saturating_sub(rows));
    }
}

#[derive(Clone, Debug)]
pub enum Modal {
    Import(ImportForm),
    Browse(Box<Browse>),
    ConfirmRemove { id: String, name: String },
    Help,
}

/// The action on the worker.
#[derive(Clone, Debug)]
pub struct Busy {
    pub serial: u64,
    pub action: Action,
    pub text: String,
    pub started: Instant,
    /// The theme that was active when an Apply started: the Undo target.
    pub previous: Option<String>,
}

/// "Applied Niru Nord. Undo" while it lasts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Undo {
    pub id: String,
    pub name: String,
    pub applied: String,
    pub until: Instant,
}

/// Something under the pointer or holding the keyboard focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Card(usize),
    Button(Button),
    Chip(Chip),
    Scrollbar,
    Undo,
    Family(usize),
    Field(usize),
    BrowseFor(usize),
    Replace,
    Submit,
    Cancel,
    Confirm,
    BrowseRow(usize),
    BrowseHome,
    BrowseDownloads,
    BrowseUp,
}

/// What the event loop must do after an input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    None,
    Redraw,
    Quit,
    Start(Action),
    Paste,
    Reload,
}

/// A key press, already resolved through the keymap.
#[derive(Clone, Copy, Debug, Default)]
pub struct Key {
    /// The keysym for the pressed level (Shift, AltGr, Caps Lock applied).
    pub sym: u32,
    /// The key's base symbol in lower case, for shortcuts.
    pub base: u32,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

pub const K_ESCAPE: u32 = 0xff1b;
pub const K_TAB: u32 = 0xff09;
pub const K_ISO_LEFT_TAB: u32 = 0xfe20;
pub const K_RETURN: u32 = 0xff0d;
pub const K_KP_ENTER: u32 = 0xff8d;
pub const K_BACKSPACE: u32 = 0xff08;
pub const K_DELETE: u32 = 0xffff;
pub const K_INSERT: u32 = 0xff63;
pub const K_HOME: u32 = 0xff50;
pub const K_LEFT: u32 = 0xff51;
pub const K_UP: u32 = 0xff52;
pub const K_RIGHT: u32 = 0xff53;
pub const K_DOWN: u32 = 0xff54;
pub const K_PAGE_UP: u32 = 0xff55;
pub const K_PAGE_DOWN: u32 = 0xff56;
pub const K_END: u32 = 0xff57;
pub const K_SPACE: u32 = 0x20;
pub const K_F1: u32 = 0xffbe;

fn letter(c: u8) -> u32 {
    u32::from(c)
}

pub struct State {
    pub catalog: Catalog,
    pub cards: Vec<Card>,
    pub palette: Palette,
    pub selected: Option<String>,
    pub focus: Option<Hit>,
    pub hover: Option<Hit>,
    pub message: String,
    pub message_is_error: bool,
    pub chip: Chip,
    pub filter: String,
    pub modal: Option<Modal>,
    pub scroll: usize,
    pub busy: Option<Busy>,
    pub undo: Option<Undo>,
    pub serial: u64,
    seen: String,
}

pub fn snapshot(catalog: &Catalog, cards: &[Card]) -> String {
    let mut out = format!(
        "{}|{}|{}|{}|",
        catalog.active, catalog.appearance_overrides, catalog.follow_theme, catalog.theme_dir
    );
    for (theme, card) in catalog.themes.iter().zip(cards) {
        out.push_str(&serde_json::to_string(theme).unwrap_or_default());
        out.push_str(&card.stamp);
        out.push(';');
    }
    out
}

pub fn build_cards(catalog: &Catalog) -> Vec<Card> {
    catalog
        .themes
        .iter()
        .map(|entry| {
            let palette = entry
                .colors
                .as_ref()
                .map(|colors| {
                    Palette::from_hex(
                        &colors.background,
                        &colors.foreground,
                        &colors.muted,
                        &colors.border,
                        &colors.focus,
                    )
                })
                .unwrap_or_else(Palette::fallback);
            let wallpaper = entry.wallpaper.as_deref().map(PathBuf::from);
            let stamp = wallpaper.as_deref().map(thumbs::key).unwrap_or_default();
            Card {
                entry: entry.clone(),
                palette,
                wallpaper,
                stamp,
            }
        })
        .collect()
}

/// Do two theme names belong to one family ("Niru Carl Larsson" and "Niru
/// Carl Larsson Midsommar")? They share at least two leading words, and one
/// name is the other plus words, or both differ only in their last word.
pub fn same_family(a: &str, b: &str) -> bool {
    let wa: Vec<&str> = a.split_whitespace().collect();
    let wb: Vec<&str> = b.split_whitespace().collect();
    let common = wa.iter().zip(&wb).take_while(|(x, y)| x == y).count();
    if common < 2 || wa == wb {
        return false;
    }
    common == wa.len() || common == wb.len() || common + 1 >= wa.len().max(wb.len())
}

impl State {
    pub fn new(catalog: Catalog, palette: Palette) -> Self {
        let cards = build_cards(&catalog);
        let seen = snapshot(&catalog, &cards);
        let mut state = Self {
            catalog,
            cards,
            palette,
            selected: None,
            focus: None,
            hover: None,
            message: "Select a theme to preview it. Apply changes the desktop.".into(),
            message_is_error: false,
            chip: Chip::All,
            filter: String::new(),
            modal: None,
            scroll: 0,
            busy: None,
            undo: None,
            serial: 0,
            seen,
        };
        state.selected = state
            .active_card()
            .map(|card| card.entry.id.clone())
            .or_else(|| {
                state
                    .visible()
                    .first()
                    .map(|i| state.cards[*i].entry.id.clone())
            });
        state
    }

    pub fn load() -> Result<Self, String> {
        Ok(Self::new(crate::backend::catalog()?, theme::load()))
    }

    pub fn set_message(&mut self, text: impl Into<String>, error: bool) {
        self.message = text.into();
        self.message_is_error = error;
    }

    /// Indices of the cards the chip and the filter let through.
    pub fn visible(&self) -> Vec<usize> {
        self.cards
            .iter()
            .enumerate()
            .filter(|(_, card)| card.matches(self.chip, &self.filter))
            .map(|(index, _)| index)
            .collect()
    }

    pub fn hidden_count(&self) -> usize {
        self.cards.iter().filter(|card| card.entry.hidden).count()
    }

    pub fn active_card(&self) -> Option<&Card> {
        self.cards
            .iter()
            .find(|card| card.entry.id == self.catalog.active)
    }

    pub fn selected_index(&self) -> Option<usize> {
        let id = self.selected.as_deref()?;
        self.cards.iter().position(|card| card.entry.id == id)
    }

    pub fn selected_card(&self) -> Option<&Card> {
        self.selected_index().map(|index| &self.cards[index])
    }

    fn position(&self, visible: &[usize], id: &str) -> Option<usize> {
        visible
            .iter()
            .position(|index| self.cards[*index].entry.id == id)
    }

    // --- scrolling -------------------------------------------------------

    pub fn total_rows(&self, l: &Layout) -> usize {
        self.visible().len().div_ceil(l.cols.max(1))
    }

    pub fn max_scroll(&self, l: &Layout) -> usize {
        self.total_rows(l).saturating_sub(l.rows.max(1))
    }

    /// The wheel moves the view by rows. It never touches the selection.
    pub fn scroll_by(&mut self, l: &Layout, rows: i64) {
        let target = self.scroll as i64 + rows;
        self.scroll = target.clamp(0, self.max_scroll(l) as i64) as usize;
    }

    pub fn clamp_scroll(&mut self, l: &Layout) {
        self.scroll = self.scroll.min(self.max_scroll(l));
    }

    /// Scroll so the selected card is in view. Called only when the
    /// selection changes through the keyboard, a click, a refresh that moved
    /// it or an import; never after the wheel.
    pub fn follow_selection(&mut self, l: &Layout) {
        let visible = self.visible();
        if let Some(position) = self
            .selected
            .as_deref()
            .and_then(|id| self.position(&visible, id))
        {
            let row = position / l.cols.max(1);
            let rows = l.rows.max(1);
            if row < self.scroll {
                self.scroll = row;
            } else if row >= self.scroll + rows {
                self.scroll = row + 1 - rows;
            }
        }
        self.clamp_scroll(l);
    }

    /// "Themes 1–9 of 23".
    pub fn position_label(&self, l: &Layout) -> String {
        let count = self.visible().len();
        if count == 0 {
            return if self.filter.is_empty() {
                "No themes".into()
            } else {
                "No themes match".into()
            };
        }
        let first = self.scroll * l.cols + 1;
        let last = ((self.scroll + l.rows) * l.cols).min(count);
        format!("Themes {first}\u{2013}{last} of {count}")
    }

    /// The scrollbar's thumb inside `l.track`, when the list does not fit.
    pub fn scroll_thumb(&self, l: &Layout) -> Option<x11rb::protocol::xproto::Rectangle> {
        let max = self.max_scroll(l);
        if max == 0 {
            return None;
        }
        let track = l.track;
        let total = self.total_rows(l).max(1) as u32;
        let thumb_h = (u32::from(track.height) * l.rows as u32 / total)
            .clamp(20.min(u32::from(track.height)), u32::from(track.height))
            as u16;
        let travel = i32::from(track.height - thumb_h);
        let offset = travel * self.scroll.min(max) as i32 / max as i32;
        Some(x11rb::protocol::xproto::Rectangle {
            y: track.y + offset as i16,
            height: thumb_h,
            ..track
        })
    }

    /// The scroll position for a thumb dragged so its top is at `top`.
    pub fn scroll_for_thumb(&self, l: &Layout, top: i16) -> usize {
        let Some(thumb) = self.scroll_thumb(l) else {
            return 0;
        };
        let max = self.max_scroll(l) as i32;
        let travel = i32::from(l.track.height.saturating_sub(thumb.height)).max(1);
        let offset = i32::from(top - l.track.y).clamp(0, travel);
        ((offset * max * 2 + travel) / (travel * 2)) as usize
    }

    // --- selection -------------------------------------------------------

    pub fn select_index(&mut self, index: usize) {
        if let Some(card) = self.cards.get(index) {
            self.selected = Some(card.entry.id.clone());
            self.set_message(
                format!("Previewing {}. Apply changes the desktop.", card.entry.name),
                false,
            );
        }
    }

    /// Move the selection among the visible cards and keep it in view.
    pub fn move_selection(&mut self, l: &Layout, delta: i64, wrap: bool) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_deref()
            .and_then(|id| self.position(&visible, id))
            .map(|p| p as i64);
        let len = visible.len() as i64;
        let next = match current {
            None => 0,
            Some(p) if wrap => (p + delta).rem_euclid(len),
            Some(p) => (p + delta).clamp(0, len - 1),
        };
        let index = visible[next as usize];
        self.select_index(index);
        self.focus = Some(Hit::Card(index));
        self.follow_selection(l);
    }

    fn select_to_end(&mut self, l: &Layout, last: bool) {
        let visible = self.visible();
        if let Some(index) = if last {
            visible.last()
        } else {
            visible.first()
        } {
            self.select_index(*index);
            self.focus = Some(Hit::Card(*index));
        }
        self.follow_selection(l);
    }

    /// After the filter or the chip changed: start at the top, and keep the
    /// selection when it still matches, else take the first match.
    fn refilter(&mut self, l: &Layout) {
        self.scroll = 0;
        let visible = self.visible();
        let keep = self
            .selected
            .as_deref()
            .is_some_and(|id| self.position(&visible, id).is_some());
        if !keep {
            if let Some(index) = visible.first() {
                self.selected = Some(self.cards[*index].entry.id.clone());
            }
        }
        if let Some(Hit::Card(index)) = self.focus {
            if !visible.contains(&index) {
                self.focus = None;
            }
        }
        self.follow_selection(l);
    }

    pub fn set_chip(&mut self, chip: Chip, l: &Layout) {
        self.chip = chip;
        self.refilter(l);
    }

    pub fn set_filter(&mut self, filter: String, l: &Layout) {
        self.filter = filter;
        self.refilter(l);
    }

    /// The other members of the selected theme's family.
    pub fn family(&self) -> Vec<usize> {
        let Some(card) = self.selected_card() else {
            return Vec::new();
        };
        self.cards
            .iter()
            .enumerate()
            .filter(|(_, other)| other.entry.id != card.entry.id)
            .filter(|(_, other)| same_family(&card.entry.name, &other.entry.name))
            .map(|(index, _)| index)
            .take(8)
            .collect()
    }

    // --- catalogue -------------------------------------------------------

    /// Take a fresh catalogue. Returns true when anything shown changed.
    /// The selection is kept when it still exists; otherwise it moves to the
    /// active theme, and the view follows it.
    pub fn apply_catalog(&mut self, catalog: Catalog, palette: Palette, l: &Layout) -> bool {
        let cards = build_cards(&catalog);
        let seen = snapshot(&catalog, &cards);
        let changed = seen != self.seen || palette != self.palette;
        let old = self.selected.clone();
        self.catalog = catalog;
        self.cards = cards;
        self.palette = palette;
        self.seen = seen;
        let visible = self.visible();
        let keep = old
            .clone()
            .filter(|id| self.position(&visible, id).is_some());
        self.selected = keep
            .or_else(|| {
                let active = self.catalog.active.clone();
                self.position(&visible, &active).map(|_| active)
            })
            .or_else(|| visible.first().map(|i| self.cards[*i].entry.id.clone()));
        if let Some(Hit::Card(index)) = self.focus {
            if !visible.contains(&index) {
                self.focus = None;
            }
        }
        if self.selected != old {
            self.follow_selection(l);
        } else {
            self.clamp_scroll(l);
        }
        changed
    }

    // --- actions ---------------------------------------------------------

    /// Why a button cannot be used now, or `None` when it can.
    pub fn disabled_reason(&self, button: Button) -> Option<String> {
        if self.busy.is_some() {
            return Some("Wait for the current action to finish.".into());
        }
        let card = self.selected_card();
        let active = card.is_some_and(|card| card.entry.id == self.catalog.active);
        match button {
            Button::Apply => match card {
                None => Some("Select a theme first.".into()),
                Some(card) if !card.entry.valid => Some(format!(
                    "This theme cannot be applied: {}",
                    card.entry.error.as_deref().unwrap_or("it is invalid")
                )),
                _ => None,
            },
            Button::Remove => match card {
                None => Some("Select a theme first.".into()),
                Some(card) if card.entry.builtin && !card.entry.overridden => {
                    Some("Built-in themes can be hidden, not removed.".into())
                }
                Some(_) if active => {
                    Some("Apply another theme before removing the active one.".into())
                }
                _ => None,
            },
            Button::Hide => match card {
                None => Some("Select a theme first.".into()),
                Some(card) if active && !card.entry.hidden => {
                    Some("Apply another theme before hiding the active one.".into())
                }
                _ => None,
            },
            Button::Import | Button::Reload => None,
        }
    }

    /// Start `action` on the worker: returns the command, or nothing while
    /// another action runs (so a second click cannot apply twice).
    pub fn start(&mut self, action: Action, now: Instant) -> Command {
        if self.busy.is_some() {
            return Command::None;
        }
        self.serial += 1;
        let previous = matches!(action, Action::Apply { .. }).then(|| self.catalog.active.clone());
        let text = action.busy_text();
        self.set_message(text.clone(), false);
        if matches!(action, Action::Apply { .. } | Action::Restore { .. }) {
            self.undo = None;
        }
        self.busy = Some(Busy {
            serial: self.serial,
            action: action.clone(),
            text,
            started: now,
            previous,
        });
        Command::Start(action)
    }

    fn press(&mut self, button: Button, now: Instant) -> Command {
        if let Some(reason) = self.disabled_reason(button) {
            self.set_message(reason, true);
            return Command::Redraw;
        }
        let card = self.selected_card().cloned();
        match button {
            Button::Apply => {
                let Some(card) = card else {
                    return Command::None;
                };
                self.start(
                    Action::Apply {
                        id: card.entry.id.clone(),
                        name: card.entry.name.clone(),
                    },
                    now,
                )
            }
            Button::Import => {
                self.modal = Some(Modal::Import(ImportForm::default()));
                Command::Redraw
            }
            Button::Remove => {
                let Some(card) = card else {
                    return Command::None;
                };
                self.modal = Some(Modal::ConfirmRemove {
                    id: card.entry.id.clone(),
                    name: card.entry.name.clone(),
                });
                Command::Redraw
            }
            Button::Hide => {
                let Some(card) = card else {
                    return Command::None;
                };
                self.start(
                    Action::Hide {
                        id: card.entry.id.clone(),
                        name: card.entry.name.clone(),
                        hide: !card.entry.hidden,
                    },
                    now,
                )
            }
            Button::Reload => Command::Reload,
        }
    }

    /// Re-apply the theme that was active before the last Apply.
    pub fn undo(&mut self, now: Instant) -> Command {
        let Some(undo) = self.undo.clone().filter(|u| u.until > now) else {
            return Command::None;
        };
        self.start(
            Action::Restore {
                id: undo.id,
                name: undo.name,
            },
            now,
        )
    }

    /// The worker's answer. `serial` must be the running action's: a late
    /// answer (after the window gave up waiting) changes nothing.
    pub fn finish(
        &mut self,
        serial: u64,
        result: Result<(), String>,
        catalog: Option<Result<Catalog, String>>,
        palette: Palette,
        l: &Layout,
        now: Instant,
    ) -> bool {
        let Some(busy) = self.busy.take_if(|busy| busy.serial == serial) else {
            return false;
        };
        match &catalog {
            Some(Ok(catalog)) => {
                self.apply_catalog(catalog.clone(), palette, l);
            }
            Some(Err(error)) if result.is_ok() => self.set_message(error.clone(), true),
            _ => {}
        }
        let error = result.err();
        if let Some(error) = error {
            if let Some(Modal::Import(form)) = self.modal.as_mut() {
                form.error = Some(error.clone());
            }
            self.set_message(error, true);
            return true;
        }
        match &busy.action {
            Action::Apply { id, name } => {
                self.set_message(format!("Applied {name}."), false);
                self.undo = undo_target(&self.cards, busy.previous.as_deref(), id, name, now);
            }
            Action::Restore { name, .. } => {
                self.set_message(format!("Restored {name}."), false);
            }
            Action::Import { .. } => {
                let form = match self.modal.take() {
                    Some(Modal::Import(form)) => Some(form),
                    other => {
                        self.modal = other;
                        None
                    }
                };
                let id = form.as_ref().and_then(ImportForm::imported_id);
                if let Some(index) = id
                    .as_deref()
                    .and_then(|id| self.cards.iter().position(|c| c.entry.id == id))
                {
                    if !self.visible().contains(&index) {
                        self.chip = Chip::All;
                        self.filter.clear();
                    }
                    self.selected = Some(self.cards[index].entry.id.clone());
                    self.focus = Some(Hit::Card(index));
                    self.follow_selection(l);
                    let name = self.cards[index].entry.name.clone();
                    self.set_message(format!("Imported {name}."), false);
                } else {
                    self.set_message("Imported the theme.", false);
                }
            }
            Action::Remove { name, .. } => self.set_message(format!("Removed {name}."), false),
            Action::Hide { name, hide, .. } => self.set_message(
                if *hide {
                    format!("Hid {name}. It stays installed; the Hidden chip shows it.")
                } else {
                    format!("Showing {name} again.")
                },
                false,
            ),
        }
        true
    }

    /// Give up waiting for the worker; its late answer will be ignored.
    pub fn abandon(&mut self) {
        if let Some(busy) = self.busy.take() {
            self.set_message(
                format!(
                    "{} took too long and was given up.",
                    busy.text.trim_end_matches('\u{2026}')
                ),
                true,
            );
        }
    }

    pub fn expire(&mut self, now: Instant) -> bool {
        if self.undo.as_ref().is_some_and(|u| u.until <= now) {
            self.undo = None;
            return true;
        }
        false
    }

    // --- keyboard --------------------------------------------------------

    fn focus_order(&self) -> Vec<Hit> {
        let mut order: Vec<Hit> = self.visible().into_iter().map(Hit::Card).collect();
        order.extend(crate::layout::BUTTONS.map(Hit::Button));
        order.extend(crate::layout::CHIPS.map(Hit::Chip));
        order
    }

    fn next_focus(&mut self, backward: bool, l: &Layout) {
        let order = self.focus_order();
        if order.is_empty() {
            return;
        }
        let index = self
            .focus
            .and_then(|hit| order.iter().position(|candidate| *candidate == hit));
        let next = match (index, backward) {
            (Some(i), true) => (i + order.len() - 1) % order.len(),
            (Some(i), false) => (i + 1) % order.len(),
            (None, true) => order.len() - 1,
            (None, false) => self
                .selected_index()
                .and_then(|s| order.iter().position(|h| *h == Hit::Card(s)))
                .map(|p| (p + 1) % order.len())
                .unwrap_or(0),
        };
        self.focus = Some(order[next]);
        if let Some(Hit::Card(index)) = self.focus {
            self.select_index(index);
            self.follow_selection(l);
        }
    }

    /// Activate a control (a click, Enter or Space on it).
    pub fn activate(&mut self, hit: Hit, l: &Layout, now: Instant) -> Command {
        match hit {
            Hit::Card(index) => {
                self.select_index(index);
                self.focus = Some(Hit::Card(index));
                self.follow_selection(l);
                Command::Redraw
            }
            Hit::Family(index) => {
                if !self.visible().contains(&index) {
                    self.chip = if self.cards[index].entry.hidden {
                        Chip::Hidden
                    } else {
                        Chip::All
                    };
                    self.filter.clear();
                }
                self.select_index(index);
                self.follow_selection(l);
                Command::Redraw
            }
            Hit::Button(button) => self.press(button, now),
            Hit::Chip(chip) => {
                self.set_chip(chip, l);
                self.focus = Some(Hit::Chip(chip));
                Command::Redraw
            }
            Hit::Undo => self.undo(now),
            Hit::Confirm => self.confirm_remove(now),
            Hit::Cancel => {
                self.modal = match self.modal.take() {
                    Some(Modal::Browse(browse)) => Some(Modal::Import(browse.form)),
                    _ => None,
                };
                Command::Redraw
            }
            Hit::Field(index) => {
                if let Some(Modal::Import(form)) = self.modal.as_mut() {
                    form.focus = index;
                }
                Command::Redraw
            }
            Hit::Replace => {
                if let Some(Modal::Import(form)) = self.modal.as_mut() {
                    form.replace = !form.replace;
                }
                Command::Redraw
            }
            Hit::Submit => self.submit_import(now),
            Hit::BrowseFor(field) => {
                if let Some(Modal::Import(form)) = self.modal.take() {
                    let want = if field == WALLPAPER_FIELD {
                        Want::Wallpaper
                    } else {
                        Want::Theme
                    };
                    let home = import::home();
                    let current = crate::backend::expand_home(form.fields[field].text.trim());
                    let start = current
                        .parent()
                        .filter(|p| !form.fields[field].text.trim().is_empty() && p.is_dir())
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| import::start_dir(home.as_deref()));
                    self.modal = Some(Modal::Browse(Box::new(Browse::open(form, want, start))));
                }
                Command::Redraw
            }
            Hit::BrowseRow(index) => {
                if let Some(Modal::Browse(browse)) = self.modal.as_mut() {
                    browse.selected = index;
                }
                Command::Redraw
            }
            Hit::BrowseHome | Hit::BrowseDownloads | Hit::BrowseUp => {
                if let Some(Modal::Browse(browse)) = self.modal.as_mut() {
                    let home = import::home().unwrap_or_else(|| PathBuf::from("/"));
                    match hit {
                        Hit::BrowseHome => browse.go(home),
                        Hit::BrowseDownloads => browse.go(home.join("Downloads")),
                        _ => browse.up(),
                    }
                }
                Command::Redraw
            }
            Hit::Scrollbar => Command::None,
        }
    }

    /// Choose the selected browse row: open a folder, or take a file.
    pub fn browse_choose(&mut self, index: Option<usize>) -> Command {
        if let Some(Modal::Browse(browse)) = self.modal.take() {
            let index = index.unwrap_or(browse.selected);
            self.modal = Some(match browse.choose(index) {
                Ok(form) => Modal::Import(form),
                Err(browse) => Modal::Browse(browse),
            });
        }
        Command::Redraw
    }

    fn confirm_remove(&mut self, now: Instant) -> Command {
        if let Some(Modal::ConfirmRemove { id, name }) = self.modal.take() {
            return self.start(Action::Remove { id, name }, now);
        }
        Command::None
    }

    fn submit_import(&mut self, now: Instant) -> Command {
        if self.busy.is_some() {
            return Command::None;
        }
        let Some(Modal::Import(form)) = self.modal.as_mut() else {
            return Command::None;
        };
        match form.action() {
            Ok(action) => {
                form.error = None;
                self.start(action, now)
            }
            Err(error) => {
                form.error = Some(error);
                Command::Redraw
            }
        }
    }

    /// Text arrived from the clipboard for the focused import field.
    pub fn paste(&mut self, text: &str) {
        let line = text.lines().next().unwrap_or("").trim();
        if let Some(Modal::Import(form)) = self.modal.as_mut() {
            form.fields[form.focus].insert(line);
            form.validate();
        }
    }

    pub fn handle_key(&mut self, key: Key, l: &Layout, now: Instant) -> Command {
        if key.ctrl && key.base == letter(b'q') || key.alt && key.base == letter(b'q') {
            return Command::Quit;
        }
        match self.modal.take() {
            Some(Modal::Help) => {
                // Any key closes the help.
                return Command::Redraw;
            }
            Some(Modal::ConfirmRemove { id, name }) => {
                self.modal = Some(Modal::ConfirmRemove { id, name });
                return match key.sym {
                    K_ESCAPE => {
                        self.modal = None;
                        Command::Redraw
                    }
                    K_RETURN | K_KP_ENTER => self.confirm_remove(now),
                    _ => Command::None,
                };
            }
            Some(Modal::Import(form)) => {
                self.modal = Some(Modal::Import(form));
                return self.import_key(key, now);
            }
            Some(Modal::Browse(browse)) => {
                self.modal = Some(Modal::Browse(browse));
                return self.browse_key(key);
            }
            None => {}
        }
        if key.alt && !key.ctrl {
            return match key.base {
                b if b == letter(b'a') => self.press(Button::Apply, now),
                b if b == letter(b'i') => self.press(Button::Import, now),
                b if b == letter(b'h') => self.press(Button::Hide, now),
                b if b == letter(b'd') => self.press(Button::Remove, now),
                b if b == letter(b'r') => self.press(Button::Reload, now),
                b if b == letter(b'v') => {
                    let chip = if self.chip == Chip::Hidden {
                        Chip::All
                    } else {
                        Chip::Hidden
                    };
                    self.set_chip(chip, l);
                    Command::Redraw
                }
                _ => Command::None,
            };
        }
        if key.ctrl {
            return match key.base {
                b if b == letter(b'z') => self.undo(now),
                _ => Command::None,
            };
        }
        let grid_focus = matches!(self.focus, None | Some(Hit::Card(_)));
        match key.sym {
            K_ESCAPE => {
                if self.filter.is_empty() {
                    Command::Quit
                } else {
                    self.set_filter(String::new(), l);
                    Command::Redraw
                }
            }
            K_F1 => {
                self.modal = Some(Modal::Help);
                Command::Redraw
            }
            K_TAB | K_ISO_LEFT_TAB => {
                self.next_focus(key.shift || key.sym == K_ISO_LEFT_TAB, l);
                Command::Redraw
            }
            K_LEFT => {
                self.move_selection(l, -1, true);
                Command::Redraw
            }
            K_RIGHT => {
                self.move_selection(l, 1, true);
                Command::Redraw
            }
            K_UP => {
                self.move_selection(l, -(l.cols as i64), true);
                Command::Redraw
            }
            K_DOWN => {
                self.move_selection(l, l.cols as i64, true);
                Command::Redraw
            }
            K_PAGE_UP => {
                self.move_selection(l, -(l.page() as i64), false);
                Command::Redraw
            }
            K_PAGE_DOWN => {
                self.move_selection(l, l.page() as i64, false);
                Command::Redraw
            }
            K_HOME => {
                self.select_to_end(l, false);
                Command::Redraw
            }
            K_END => {
                self.select_to_end(l, true);
                Command::Redraw
            }
            K_RETURN | K_KP_ENTER => match self.focus {
                Some(hit @ (Hit::Button(_) | Hit::Chip(_))) => self.activate(hit, l, now),
                // Enter on a card (or with nothing focused) applies the
                // selected theme, as the footer says.
                _ => self.press(Button::Apply, now),
            },
            K_SPACE if self.filter.is_empty() || !grid_focus => match self.focus {
                Some(hit) => self.activate(hit, l, now),
                None => Command::None,
            },
            K_BACKSPACE if grid_focus => {
                let mut filter = self.filter.clone();
                if filter.pop().is_some() {
                    self.set_filter(filter, l);
                }
                Command::Redraw
            }
            sym if grid_focus => match crate::keyboard::text_char(sym) {
                Some(c) => {
                    let mut filter = self.filter.clone();
                    if filter.chars().count() < 64 {
                        filter.push(c);
                    }
                    self.set_filter(filter, l);
                    Command::Redraw
                }
                None => Command::None,
            },
            _ => Command::None,
        }
    }

    fn import_key(&mut self, key: Key, now: Instant) -> Command {
        let busy = self.busy.is_some();
        let Some(Modal::Import(form)) = self.modal.as_mut() else {
            return Command::None;
        };
        if key.sym == K_ESCAPE {
            self.modal = None;
            return Command::Redraw;
        }
        if busy {
            return Command::None;
        }
        let field = form.focus;
        let paste = key.ctrl && key.base == letter(b'v') || key.shift && key.sym == K_INSERT;
        if paste {
            return Command::Paste;
        }
        if key.ctrl && key.base == letter(b'r') {
            form.replace = !form.replace;
            return Command::Redraw;
        }
        if key.ctrl && key.base == letter(b'b') {
            let field = if form.focus == ID_FIELD {
                THEME_FIELD
            } else {
                form.focus
            };
            return self.activate(Hit::BrowseFor(field), &crate::layout::layout(0, 0), now);
        }
        let mut edited = false;
        match key.sym {
            K_TAB | K_ISO_LEFT_TAB => {
                form.focus = if key.shift || key.sym == K_ISO_LEFT_TAB {
                    (field + 2) % 3
                } else {
                    (field + 1) % 3
                };
            }
            K_RETURN | K_KP_ENTER => return self.submit_import(now),
            K_LEFT => form.fields[field].left(),
            K_RIGHT => form.fields[field].right(),
            K_HOME => form.fields[field].home(),
            K_END => form.fields[field].end(),
            K_BACKSPACE => {
                form.fields[field].backspace();
                edited = true;
            }
            K_DELETE => {
                form.fields[field].delete();
                edited = true;
            }
            sym if !key.ctrl && !key.alt => {
                if let Some(c) = crate::keyboard::text_char(sym) {
                    form.fields[field].insert(&c.to_string());
                    edited = true;
                } else {
                    return Command::None;
                }
            }
            _ => return Command::None,
        }
        if edited {
            form.error = None;
            form.validate();
        }
        Command::Redraw
    }

    fn browse_key(&mut self, key: Key) -> Command {
        let Some(Modal::Browse(browse)) = self.modal.as_mut() else {
            return Command::None;
        };
        let count = browse.entries.len();
        let rows = 10;
        match key.sym {
            K_ESCAPE => {
                if let Some(Modal::Browse(browse)) = self.modal.take() {
                    self.modal = Some(Modal::Import(browse.form));
                }
            }
            K_UP => browse.selected = browse.selected.saturating_sub(1),
            K_DOWN => browse.selected = (browse.selected + 1).min(count.saturating_sub(1)),
            K_PAGE_UP => browse.selected = browse.selected.saturating_sub(rows),
            K_PAGE_DOWN => browse.selected = (browse.selected + rows).min(count.saturating_sub(1)),
            K_HOME => browse.selected = 0,
            K_END => browse.selected = count.saturating_sub(1),
            K_BACKSPACE => browse.up(),
            K_RETURN | K_KP_ENTER => return self.browse_choose(None),
            _ if key.alt && key.base == letter(b'h') => {
                if let Some(home) = import::home() {
                    browse.go(home);
                }
            }
            _ if key.alt && key.base == letter(b'd') => {
                if let Some(home) = import::home() {
                    browse.go(home.join("Downloads"));
                }
            }
            _ => return Command::None,
        }
        Command::Redraw
    }
}

/// The Undo offered after applying `applied`: the theme that was active
/// before, when it differs and still exists.
pub fn undo_target(
    cards: &[Card],
    previous: Option<&str>,
    applied: &str,
    applied_name: &str,
    now: Instant,
) -> Option<Undo> {
    let previous = previous.filter(|p| !p.is_empty() && *p != applied)?;
    let card = cards.iter().find(|card| card.entry.id == previous)?;
    Some(Undo {
        id: previous.to_string(),
        name: card.entry.name.clone(),
        applied: applied_name.to_string(),
        until: now + UNDO_FOR,
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::{backend::Colors, layout::layout};

    pub fn catalog(count: usize) -> Catalog {
        let themes = (1..=count)
            .map(|i| ThemeEntry {
                id: format!("t{i:02}"),
                name: if i <= 3 {
                    format!("Niru Carl Larsson{}", ["", " Midsommar", " Vinter"][i - 1])
                } else {
                    format!("Theme {i:02}")
                },
                builtin: i % 2 == 1,
                overridden: false,
                hidden: i == 5 || i == 6,
                valid: i != 7,
                error: (i == 7).then(|| "bad colour".to_string()),
                colors: Some(Colors {
                    name: String::new(),
                    background: "101010".into(),
                    foreground: "eeeeee".into(),
                    muted: "999999".into(),
                    border: "333333".into(),
                    focus: "88c0d0".into(),
                }),
                wallpaper: None,
                wallpaper_available: false,
            })
            .collect();
        Catalog {
            schema: 1,
            active: "t01".into(),
            themes,
            ..Catalog::default()
        }
    }

    fn state(count: usize) -> State {
        State::new(catalog(count), Palette::fallback())
    }

    fn key(sym: u32) -> Key {
        Key {
            sym,
            base: sym,
            ..Key::default()
        }
    }

    #[test]
    fn the_wheel_scrolls_to_the_end_and_never_moves_the_selection() {
        let mut s = state(25);
        let l = layout(1120, 760);
        assert_eq!((l.cols, l.rows), (3, 3));
        // 23 visible themes (two hidden): 8 rows, 3 in view.
        assert_eq!(s.visible().len(), 23);
        assert_eq!(s.max_scroll(&l), 5);
        for _ in 0..12 {
            s.scroll_by(&l, 1);
            // The event loop no longer pulls the view back to the
            // selection after each event.
            s.clamp_scroll(&l);
        }
        assert_eq!(s.scroll, 5, "the last row is reachable and the bound holds");
        assert_eq!(
            s.selected.as_deref(),
            Some("t01"),
            "the wheel selected something"
        );
        assert_eq!(s.position_label(&l), "Themes 16\u{2013}23 of 23");
        for _ in 0..20 {
            s.scroll_by(&l, -1);
        }
        assert_eq!(s.scroll, 0);
        // The keyboard does follow the selection.
        s.scroll = 5;
        s.move_selection(&l, 1, true);
        assert_eq!(s.selected.as_deref(), Some("t02"));
        assert_eq!(s.scroll, 0);
        s.handle_key(key(K_END), &l, Instant::now());
        assert_eq!(s.selected.as_deref(), Some("t25"));
        assert_eq!(s.scroll, 5);
        s.handle_key(key(K_PAGE_UP), &l, Instant::now());
        // PageUp moves nine cards (three rows): position 22 to 13, row 4.
        assert_eq!(s.scroll, 4);
        s.handle_key(key(K_HOME), &l, Instant::now());
        assert_eq!((s.selected.as_deref(), s.scroll), (Some("t01"), 0));
    }

    #[test]
    fn the_scrollbar_thumb_tracks_and_drags_the_scroll() {
        let mut s = state(30);
        let l = layout(1120, 760);
        let max = s.max_scroll(&l);
        let top = s.scroll_thumb(&l).unwrap();
        assert_eq!(top.y, l.track.y);
        s.scroll = max;
        let bottom = s.scroll_thumb(&l).unwrap();
        assert_eq!(
            bottom.y + bottom.height as i16,
            l.track.y + l.track.height as i16
        );
        assert_eq!(s.scroll_for_thumb(&l, l.track.y), 0);
        assert_eq!(s.scroll_for_thumb(&l, bottom.y + 100), max);
        assert!(state(4).scroll_thumb(&l).is_none(), "no bar when all fit");
    }

    #[test]
    fn typing_filters_and_escape_clears_before_closing() {
        let mut s = state(25);
        let l = layout(1120, 760);
        let now = Instant::now();
        for c in "larsson".bytes() {
            assert_eq!(s.handle_key(key(u32::from(c)), &l, now), Command::Redraw);
        }
        assert_eq!(s.filter, "larsson");
        assert_eq!(s.visible(), vec![0, 1, 2]);
        // Space extends a filter once one is typed.
        s.handle_key(key(K_SPACE), &l, now);
        s.handle_key(key(u32::from(b'v')), &l, now);
        assert_eq!(s.visible(), vec![2], "\"larsson v\" matches Vinter only");
        assert_eq!(s.selected.as_deref(), Some("t03"), "the selection follows");
        // Letters filter; they are not shortcuts any more. Alt+letter is.
        assert!(s.busy.is_none());
        assert_eq!(s.handle_key(key(K_ESCAPE), &l, now), Command::Redraw);
        assert!(s.filter.is_empty());
        assert_eq!(s.handle_key(key(K_ESCAPE), &l, now), Command::Quit);
        // Matching uses the id too.
        s.set_filter("t1".into(), &l);
        assert_eq!(s.visible().len(), 10);
        s.set_filter("nothing".into(), &l);
        assert_eq!(s.position_label(&l), "No themes match");
    }

    #[test]
    fn the_chips_split_built_in_user_and_hidden() {
        let mut s = state(10);
        let l = layout(1120, 760);
        assert_eq!(s.hidden_count(), 2);
        s.set_chip(Chip::Builtin, &l);
        assert!(s.visible().iter().all(|i| s.cards[*i].entry.builtin));
        assert!(!s.visible().contains(&4), "t05 is hidden");
        s.set_chip(Chip::User, &l);
        assert!(s.visible().iter().all(|i| !s.cards[*i].entry.builtin));
        s.set_chip(Chip::Hidden, &l);
        assert_eq!(s.visible(), vec![4, 5]);
        assert_eq!(
            s.selected.as_deref(),
            Some("t05"),
            "selection moved into the chip"
        );
        // Alt+V toggles the Hidden chip, as V used to toggle hidden themes.
        let alt_v = Key {
            sym: u32::from(b'v'),
            base: u32::from(b'v'),
            alt: true,
            ..Key::default()
        };
        s.handle_key(alt_v, &l, Instant::now());
        assert_eq!(s.chip, Chip::All);
        s.handle_key(alt_v, &l, Instant::now());
        assert_eq!(s.chip, Chip::Hidden);
    }

    #[test]
    fn undo_reapplies_the_previously_active_theme() {
        let mut s = state(10);
        let l = layout(1120, 760);
        let now = Instant::now();
        s.select_index(3);
        // Enter on the focused card applies it.
        s.focus = Some(Hit::Card(3));
        let command = s.handle_key(key(K_RETURN), &l, now);
        assert_eq!(
            command,
            Command::Start(Action::Apply {
                id: "t04".into(),
                name: "Theme 04".into()
            })
        );
        assert_eq!(s.message, "Applying Theme 04\u{2026}");
        assert_eq!(
            s.disabled_reason(Button::Apply).as_deref(),
            Some("Wait for the current action to finish.")
        );
        // A second Apply while the first runs does nothing.
        assert_eq!(s.handle_key(key(K_RETURN), &l, now), Command::Redraw);
        let serial = s.busy.as_ref().unwrap().serial;
        // A stale answer is ignored.
        assert!(!s.finish(serial + 7, Ok(()), None, Palette::fallback(), &l, now));
        let mut after = catalog(10);
        after.active = "t04".into();
        assert!(s.finish(
            serial,
            Ok(()),
            Some(Ok(after)),
            Palette::fallback(),
            &l,
            now
        ));
        assert_eq!(s.message, "Applied Theme 04.");
        let undo = s.undo.clone().expect("an undo is offered");
        assert_eq!(
            (undo.id.as_str(), undo.name.as_str()),
            ("t01", "Niru Carl Larsson")
        );
        assert_eq!(undo.until, now + UNDO_FOR);
        // Ctrl+Z (or the Undo link) re-applies t01 through the worker.
        let ctrl_z = Key {
            sym: u32::from(b'z'),
            base: u32::from(b'z'),
            ctrl: true,
            ..Key::default()
        };
        assert_eq!(
            s.handle_key(ctrl_z, &l, now + Duration::from_secs(1)),
            Command::Start(Action::Restore {
                id: "t01".into(),
                name: "Niru Carl Larsson".into()
            })
        );
        assert!(s.undo.is_none());
        // No undo after the toast expired, or for re-applying the active one.
        assert!(undo_target(&s.cards, Some("t01"), "t01", "x", now).is_none());
        assert!(undo_target(&s.cards, Some("gone"), "t01", "x", now).is_none());
        let mut late = state(10);
        late.undo = Some(Undo { until: now, ..undo });
        assert_eq!(late.undo(now + Duration::from_secs(16)), Command::None);
        assert!(late.expire(now + Duration::from_secs(16)));
    }

    #[test]
    fn a_failed_apply_keeps_the_old_state_and_offers_no_undo() {
        let mut s = state(10);
        let l = layout(1120, 760);
        let now = Instant::now();
        s.select_index(3);
        s.handle_key(key(K_RETURN), &l, now);
        let serial = s.busy.as_ref().unwrap().serial;
        s.finish(
            serial,
            Err("ffmpeg failed".into()),
            Some(Ok(catalog(10))),
            Palette::fallback(),
            &l,
            now,
        );
        assert!(s.busy.is_none() && s.undo.is_none());
        assert!(s.message_is_error);
        assert_eq!(s.catalog.active, "t01");
    }

    #[test]
    fn disabled_buttons_explain_why() {
        let mut s = state(10);
        s.select_index(0);
        assert_eq!(
            s.disabled_reason(Button::Remove).as_deref(),
            Some("Built-in themes can be hidden, not removed.")
        );
        let mut user_active = catalog(10);
        user_active.active = "t02".into();
        s = State::new(user_active, Palette::fallback());
        assert_eq!(
            s.disabled_reason(Button::Remove).as_deref(),
            Some("Apply another theme before removing the active one.")
        );
        s.select_index(6);
        assert!(s
            .disabled_reason(Button::Apply)
            .unwrap()
            .contains("bad colour"));
        s.select_index(3);
        assert_eq!(s.disabled_reason(Button::Remove), None);
    }

    #[test]
    fn a_refresh_keeps_the_view_unless_the_selection_moved() {
        let mut s = state(30);
        let l = layout(1120, 760);
        s.scroll_by(&l, 4);
        assert!(!s.apply_catalog(catalog(30), Palette::fallback(), &l));
        assert_eq!(s.scroll, 4, "an unchanged refresh must not snap back");
        // The selected theme disappeared: the selection moves to the active
        // theme and the view follows it.
        let mut fewer = catalog(30);
        fewer.themes.retain(|t| t.id != "t01");
        fewer.active = "t29".into();
        assert!(s.apply_catalog(fewer, Palette::fallback(), &l));
        assert_eq!(s.selected.as_deref(), Some("t29"));
        assert_eq!(s.scroll, s.max_scroll(&l));
    }

    #[test]
    fn family_members_share_a_name_prefix() {
        assert!(same_family(
            "Niru Carl Larsson",
            "Niru Carl Larsson Midsommar"
        ));
        assert!(same_family(
            "Niru Carl Larsson Vinter",
            "Niru Carl Larsson Midsommar"
        ));
        assert!(same_family("Niru Satie", "Niru Satie Gnossienne No 1"));
        assert!(!same_family("Niru Nord", "Niru Noir"));
        assert!(!same_family("Niru Blue Horizon", "Niru Blood of Urdr"));
        assert!(!same_family("Niru Nord", "Niru Nord"));
        let mut s = state(10);
        s.select_index(1);
        assert_eq!(s.family(), vec![0, 2]);
        s.select_index(4);
        assert!(s.family().is_empty());
    }

    #[test]
    fn fields_edit_at_the_cursor() {
        let mut f = Field::default();
        f.insert("~/Teman/x.toml");
        f.home();
        f.right();
        f.right();
        f.insert("Å");
        assert_eq!(f.text, "~/ÅTeman/x.toml");
        f.backspace();
        f.delete();
        assert_eq!(f.text, "~/eman/x.toml");
        f.end();
        f.left();
        f.backspace();
        assert_eq!(f.text, "~/eman/x.tol");
        f.insert("line\nbreak");
        assert!(!f.text.contains('\n'));
    }

    #[test]
    fn the_import_form_refuses_what_niructl_would_refuse() {
        let dir = std::env::temp_dir().join(format!("nts-form-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let theme = dir.join("mine.toml");
        std::fs::write(
            &theme,
            "name = \"Mine\"\nbackground = \"000000\"\nforeground = \"ffffff\"\nmuted = \"888888\"\nborder = \"222222\"\nfocus = \"ff0000\"\n",
        )
        .unwrap();
        let mut form = ImportForm::default();
        assert!(form.action().unwrap_err().contains("path"));
        form.fields[THEME_FIELD].set(dir.join("absent.toml").display().to_string());
        assert!(form.action().unwrap_err().contains("not found"));
        form.fields[THEME_FIELD].set(theme.display().to_string());
        form.validate();
        assert_eq!(form.preview.clone().unwrap().unwrap().name, "Mine");
        let jpeg = dir.join("w.jpg");
        image::RgbImage::new(2, 2).save(&jpeg).unwrap();
        form.fields[WALLPAPER_FIELD].set(jpeg.display().to_string());
        assert!(form.action().unwrap_err().contains("PNG"));
        form.fields[WALLPAPER_FIELD].set(String::new());
        form.fields[ID_FIELD].set("bad id".into());
        assert!(form.action().is_err());
        form.fields[ID_FIELD].set("my-theme".into());
        assert!(matches!(form.action(), Ok(Action::Import { .. })));
        assert_eq!(form.imported_id().as_deref(), Some("my-theme"));
        // Browse picks a file and returns to the form with it filled in.
        let browse = Browse::open(form, Want::Theme, dir.clone());
        assert_eq!(browse.entries.len(), 1);
        let form = browse.choose(0).expect("a file returns the form");
        assert!(form.fields[THEME_FIELD].text.ends_with("mine.toml"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
