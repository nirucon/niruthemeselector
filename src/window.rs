//! The NIRU Theme Selector window.
//!
//! A native immediate-mode X11 window. The catalogue, application and import
//! rules live in niructl; this file only presents them and keeps a local
//! preview selection.

use crate::{
    backend::{self, Catalog, ThemeEntry},
    text,
    theme::{self, Palette},
    thumbs,
};
use image::RgbaImage;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{xproto::*, Event},
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
    COPY_DEPTH_FROM_PARENT,
};

pub const APP_ID: &str = "niruthemeselector";
pub const APP_NAME: &str = "NIRU Theme Selector";

const WIDTH: u16 = 1120;
const HEIGHT: u16 = 760;
const MIN_WIDTH: u16 = 900;
const MIN_HEIGHT: u16 = 620;
const PAD: i16 = 20;
const HEADER: i16 = 76;
const FOOTER: i16 = 56;
const ACTIONS: i16 = 62;
const PANEL_W: i16 = 300;
const CARD_H: i16 = 164;
const GAP: i16 = 12;

const K_ESCAPE: u32 = 0xff1b;
const K_TAB: u32 = 0xff09;
const K_RETURN: u32 = 0xff0d;
const K_BACKSPACE: u32 = 0xff08;
const K_LEFT: u32 = 0xff51;
const K_UP: u32 = 0xff52;
const K_RIGHT: u32 = 0xff53;
const K_DOWN: u32 = 0xff54;
const K_SPACE: u32 = 0x0020;
const K_F1: u32 = 0xffbe;
const K_Q: u32 = b'q' as u32;
const K_Q_SHIFTED: u32 = b'Q' as u32;
const K_A: u32 = b'a' as u32;
const K_I: u32 = b'i' as u32;
const K_R: u32 = b'r' as u32;
const K_D: u32 = b'd' as u32;
const K_H: u32 = b'h' as u32;
const K_V: u32 = b'v' as u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Card(usize),
    Apply,
    Import,
    Remove,
    Hide,
    ToggleHidden,
    Reload,
    ImportField(ImportField),
    ImportReplace,
    Confirm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportField {
    Theme,
    Wallpaper,
    Id,
}

#[derive(Clone, Debug)]
pub struct Card {
    pub entry: ThemeEntry,
    pub palette: Palette,
    pub wallpaper: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub enum Modal {
    Import {
        theme: String,
        wallpaper: String,
        id: String,
        field: ImportField,
        replace: bool,
    },
    ConfirmRemove {
        id: String,
        name: String,
    },
    Help,
}

pub struct State {
    pub catalog: Catalog,
    pub cards: Vec<Card>,
    pub selected: Option<String>,
    pub focus: Option<Hit>,
    pub message: String,
    pub message_is_error: bool,
    pub show_hidden: bool,
    pub modal: Option<Modal>,
    pub scroll: usize,
    pub palette: Palette,
    pub thumbs: HashMap<PathBuf, Option<RgbaImage>>,
    requested: HashSet<PathBuf>,
    tx: Sender<PathBuf>,
    rx: Receiver<(PathBuf, Option<RgbaImage>)>,
    seen: String,
}

impl State {
    pub fn load() -> Result<Self, String> {
        let (request_tx, request_rx) = mpsc::channel::<PathBuf>();
        let (result_tx, result_rx) = mpsc::channel::<(PathBuf, Option<RgbaImage>)>();
        spawn_workers(request_rx, result_tx);
        let catalog = backend::catalog()?;
        let cards = build_cards(&catalog);
        let selected = cards
            .iter()
            .find(|card| card.entry.id == catalog.active)
            .map(|card| card.entry.id.clone())
            .or_else(|| cards.first().map(|card| card.entry.id.clone()));
        let seen = snapshot(&catalog);
        let mut state = Self {
            catalog,
            cards,
            selected,
            focus: None,
            message: "Select a theme to preview it. Apply changes the desktop.".into(),
            message_is_error: false,
            show_hidden: false,
            modal: None,
            scroll: 0,
            palette: theme::load(),
            thumbs: HashMap::new(),
            requested: HashSet::new(),
            tx: request_tx,
            rx: result_rx,
            seen,
        };
        state.request_visible();
        Ok(state)
    }

    fn visible_indices(&self) -> Vec<usize> {
        self.cards
            .iter()
            .enumerate()
            .filter(|(_, card)| self.show_hidden || !card.entry.hidden)
            .map(|(index, _)| index)
            .collect()
    }

    fn selected_card(&self) -> Option<&Card> {
        let id = self.selected.as_deref()?;
        self.cards.iter().find(|card| card.entry.id == id)
    }

    fn visible_position(&self, id: &str) -> Option<usize> {
        self.visible_indices()
            .into_iter()
            .position(|index| self.cards[index].entry.id == id)
    }

    fn set_message(&mut self, text: impl Into<String>, error: bool) {
        self.message = text.into();
        self.message_is_error = error;
    }

    pub fn refresh(&mut self) -> Result<bool, String> {
        let catalog = backend::catalog()?;
        let changed = snapshot(&catalog) != self.seen;
        let old_selected = self.selected.clone();
        self.catalog = catalog;
        self.cards = build_cards(&self.catalog);
        self.palette = theme::load();
        let visible = self.visible_indices();
        if visible.is_empty() {
            self.selected = None;
        } else {
            let keep = old_selected
                .filter(|id| {
                    visible
                        .iter()
                        .any(|index| &self.cards[*index].entry.id == id)
                })
                .or_else(|| {
                    self.cards
                        .iter()
                        .find(|card| card.entry.id == self.catalog.active)
                        .map(|card| card.entry.id.clone())
                })
                .or_else(|| Some(self.cards[visible[0]].entry.id.clone()));
            self.selected = keep;
        }
        self.seen = snapshot(&self.catalog);
        if changed {
            self.request_visible();
        }
        Ok(changed)
    }

    fn request_visible(&mut self) {
        for index in self.visible_indices() {
            let Some(path) = self.cards[index].wallpaper.clone() else {
                continue;
            };
            if self.thumbs.contains_key(&path) || self.requested.contains(&path) {
                continue;
            }
            if self.tx.send(path.clone()).is_ok() {
                self.requested.insert(path);
            }
        }
    }

    fn collect_thumbs(&mut self) -> bool {
        let mut changed = false;
        while let Ok((path, image)) = self.rx.try_recv() {
            self.thumbs.insert(path, image);
            changed = true;
        }
        changed
    }

    fn ensure_visible(&mut self, cols: usize, rows: usize) {
        let Some(id) = self.selected.as_deref() else {
            return;
        };
        let Some(position) = self.visible_position(id) else {
            return;
        };
        let row = position / cols.max(1);
        if row < self.scroll {
            self.scroll = row;
        } else if row >= self.scroll + rows.max(1) {
            self.scroll = row + 1 - rows.max(1);
        }
    }

    fn select(&mut self, id: String) {
        self.selected = Some(id);
        self.focus = None;
        self.set_message("Previewing this theme. Apply changes the desktop.", false);
    }

    fn apply(&mut self) -> Result<(), String> {
        let id = self
            .selected_card()
            .map(|card| card.entry.id.clone())
            .ok_or("select a theme first")?;
        backend::apply(&id)?;
        let _ = self.refresh()?;
        self.set_message(format!("Applied {id}."), false);
        Ok(())
    }

    fn hide_toggle(&mut self) -> Result<(), String> {
        let card = self.selected_card().ok_or("select a theme first")?;
        let id = card.entry.id.clone();
        let hide = !card.entry.hidden;
        backend::hide(&id, hide)?;
        let _ = self.refresh()?;
        self.set_message(
            if hide {
                format!("Hid {id}. It remains installed and can be shown again.")
            } else {
                format!("Showing {id} again.")
            },
            false,
        );
        Ok(())
    }

    fn remove_selected(&mut self) -> Result<(), String> {
        let (id, builtin, overridden) = self
            .selected_card()
            .map(|card| {
                (
                    card.entry.id.clone(),
                    card.entry.builtin,
                    card.entry.overridden,
                )
            })
            .ok_or("select a theme first")?;
        if builtin && !overridden {
            return Err("bundled themes are hidden, not removed".into());
        }
        backend::remove(&id)?;
        let _ = self.refresh()?;
        self.set_message(format!("Removed {id}."), false);
        Ok(())
    }

    fn import(&mut self, modal: &Modal) -> Result<Option<String>, String> {
        let Modal::Import {
            theme,
            wallpaper,
            id,
            replace,
            ..
        } = modal
        else {
            return Ok(None);
        };
        if theme.trim().is_empty() {
            return Err("enter the theme TOML path first".into());
        }
        backend::import(theme, wallpaper, id, *replace)?;
        let _ = self.refresh()?;
        let imported = if id.trim().is_empty() {
            std::path::Path::new(theme)
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
        } else {
            Some(id.trim().to_string())
        };
        if let Some(id) = &imported {
            if self.cards.iter().any(|card| &card.entry.id == id) {
                self.selected = Some(id.clone());
            }
        }
        self.set_message(
            imported
                .clone()
                .map(|id| format!("Imported {id}."))
                .unwrap_or_else(|| "Imported theme.".into()),
            false,
        );
        Ok(imported)
    }
}

fn snapshot(catalog: &Catalog) -> String {
    let mut out = format!(
        "{}|{}|{}|{}|",
        catalog.active, catalog.appearance_overrides, catalog.follow_theme, catalog.theme_dir
    );
    for theme in &catalog.themes {
        out.push_str(&format!(
            "{}:{}:{}:{}:{}:{}:{}:{};",
            theme.id,
            theme.name,
            theme.builtin,
            theme.overridden,
            theme.hidden,
            theme.valid,
            theme.wallpaper_available,
            theme.wallpaper.as_deref().unwrap_or("")
        ));
    }
    out
}

fn build_cards(catalog: &Catalog) -> Vec<Card> {
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
            Card {
                entry: entry.clone(),
                palette,
                wallpaper: entry.wallpaper.as_deref().map(PathBuf::from),
            }
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub grid: Rectangle,
    pub panel: Rectangle,
    pub actions: Rectangle,
    pub footer: Rectangle,
    pub card_w: u16,
    pub cols: usize,
    pub rows: usize,
}

fn layout(w: u16, h: u16) -> Layout {
    let width = i16::try_from(w).unwrap_or(i16::MAX);
    let height = i16::try_from(h).unwrap_or(i16::MAX);
    let grid_w = width - PAD * 2 - PANEL_W - 16;
    let grid_h = height - HEADER - FOOTER - ACTIONS - 16;
    let grid = Rectangle {
        x: PAD,
        y: HEADER,
        width: u16::try_from(grid_w).unwrap_or(0),
        height: u16::try_from(grid_h).unwrap_or(0),
    };
    let panel = Rectangle {
        x: width - PAD - PANEL_W,
        y: HEADER,
        width: PANEL_W as u16,
        height: grid.height,
    };
    let actions = Rectangle {
        x: PAD,
        y: height - FOOTER - ACTIONS,
        width: w.saturating_sub(PAD as u16 * 2),
        height: ACTIONS as u16,
    };
    let footer = Rectangle {
        x: PAD,
        y: height - FOOTER,
        width: w.saturating_sub(PAD as u16 * 2),
        height: FOOTER as u16,
    };
    let cols = if grid.width >= 760 { 3 } else { 2 };
    let card_w = (grid.width - GAP as u16 * (cols as u16 + 1)) / cols as u16;
    let rows = (grid.height / (CARD_H as u16 + GAP as u16)).max(1) as usize;
    Layout {
        grid,
        panel,
        actions,
        footer,
        card_w,
        cols,
        rows,
    }
}

fn card_rect(l: Layout, index: usize) -> Rectangle {
    let col = index % l.cols;
    let row = index / l.cols;
    Rectangle {
        x: l.grid.x + GAP + col as i16 * (i16::try_from(l.card_w + GAP as u16).unwrap_or(0)),
        y: l.grid.y + GAP + row as i16 * (CARD_H + GAP),
        width: l.card_w,
        height: CARD_H as u16,
    }
}

fn action_rect(l: Layout, index: usize) -> Rectangle {
    let width = (l.actions.width.saturating_sub(18)) / 6;
    Rectangle {
        x: l.actions.x + index as i16 * i16::try_from(width + 3).unwrap_or(0),
        y: l.actions.y + 20,
        width,
        height: 27,
    }
}

fn modal_rect(w: u16, h: u16) -> Rectangle {
    let width = 700.min(w.saturating_sub(100));
    let height = 300u16;
    Rectangle {
        x: (i16::try_from(w).unwrap_or(i16::MAX) - i16::try_from(width).unwrap_or(0)) / 2,
        y: (i16::try_from(h).unwrap_or(i16::MAX) - height as i16) / 2,
        width,
        height,
    }
}

fn field_rect(modal: Rectangle, index: usize) -> Rectangle {
    Rectangle {
        x: modal.x + 24,
        y: modal.y + 68 + index as i16 * 60,
        width: modal.width.saturating_sub(48),
        height: 28,
    }
}

fn contains(rect: Rectangle, x: i16, y: i16) -> bool {
    x >= rect.x
        && x < rect.x + i16::try_from(rect.width).unwrap_or(0)
        && y >= rect.y
        && y < rect.y + i16::try_from(rect.height).unwrap_or(0)
}

fn hit(state: &State, l: Layout, w: u16, h: u16, x: i16, y: i16) -> Option<Hit> {
    if let Some(modal) = &state.modal {
        let rect = modal_rect(w, h);
        if let Modal::Import { field, .. } = modal {
            for (index, candidate) in [ImportField::Theme, ImportField::Wallpaper, ImportField::Id]
                .into_iter()
                .enumerate()
            {
                if contains(field_rect(rect, index), x, y) {
                    return Some(Hit::ImportField(candidate));
                }
            }
            let _ = field;
            if contains(
                Rectangle {
                    x: rect.x + 24,
                    y: rect.y + 224,
                    width: 300,
                    height: 24,
                },
                x,
                y,
            ) {
                return Some(Hit::ImportReplace);
            }
        }
        if let Modal::ConfirmRemove { .. } = modal {
            if contains(
                Rectangle {
                    x: rect.x + 24,
                    y: rect.y + 170,
                    width: 170,
                    height: 30,
                },
                x,
                y,
            ) {
                return Some(Hit::Confirm);
            }
        }
        return None;
    }
    let visible = state.visible_indices();
    for (visible_index, _card_index) in visible.iter().enumerate() {
        if visible_index < state.scroll * l.cols {
            continue;
        }
        let row = visible_index / l.cols;
        if row >= state.scroll + l.rows || row < state.scroll {
            continue;
        }
        let rect = card_rect(
            Layout { grid: l.grid, ..l },
            visible_index - state.scroll * l.cols,
        );
        if contains(rect, x, y) {
            return Some(Hit::Card(card_index_index(&visible, visible_index)));
        }
    }
    for (index, hit) in [
        (0, Hit::Apply),
        (1, Hit::Import),
        (2, Hit::Remove),
        (3, Hit::Hide),
        (4, Hit::ToggleHidden),
        (5, Hit::Reload),
    ] {
        if contains(action_rect(l, index), x, y) {
            return Some(hit);
        }
    }
    None
}

fn card_index_index(visible: &[usize], visible_index: usize) -> usize {
    visible.get(visible_index).copied().unwrap_or(usize::MAX)
}

pub struct App {
    c: RustConnection,
    win: Window,
    gc: Gcontext,
    back: Pixmap,
    w: u16,
    h: u16,
    root: Window,
    depth: u8,
    image_lsb: bool,
    pub palette: Palette,
    font: text::Font,
    keys: Keys,
}

impl App {
    pub fn open() -> Result<Self, String> {
        let (c, screen) = x11rb::connect(None).map_err(|error| error.to_string())?;
        let root = c.setup().roots[screen].root;
        let depth = c
            .get_geometry(root)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?
            .depth;
        if ![24, 32].contains(&depth) {
            return Err(format!(
                "NIRU Theme Selector needs a 24 or 32-bit screen, this one is {depth}"
            ));
        }
        let image_lsb =
            c.setup().image_byte_order == x11rb::protocol::xproto::ImageOrder::LSB_FIRST;
        let palette = theme::load();
        let font = text::Font::load(palette);
        let keys = Keys::new(&c)?;
        let win = c.generate_id().map_err(|error| error.to_string())?;
        let gc = c.generate_id().map_err(|error| error.to_string())?;
        let back = c.generate_id().map_err(|error| error.to_string())?;
        c.create_window(
            COPY_DEPTH_FROM_PARENT,
            win,
            root,
            0,
            0,
            WIDTH,
            HEIGHT,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .background_pixel(palette.background)
                .event_mask(
                    EventMask::EXPOSURE
                        | EventMask::KEY_PRESS
                        | EventMask::BUTTON_PRESS
                        | EventMask::STRUCTURE_NOTIFY,
                ),
        )
        .map_err(|error| error.to_string())?;
        c.create_gc(gc, win, &CreateGCAux::new().graphics_exposures(0))
            .map_err(|error| error.to_string())?;
        c.create_pixmap(depth, back, root, WIDTH, HEIGHT)
            .map_err(|error| error.to_string())?;
        let atom = |name: &str| -> Result<Atom, String> {
            c.intern_atom(false, name.as_bytes())
                .map_err(|error| error.to_string())?
                .reply()
                .map(|reply| reply.atom)
                .map_err(|error| error.to_string())
        };
        c.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            APP_NAME.as_bytes(),
        )
        .map_err(|error| error.to_string())?;
        c.change_property8(
            PropMode::REPLACE,
            win,
            atom("_NET_WM_NAME")?,
            atom("UTF8_STRING")?,
            APP_NAME.as_bytes(),
        )
        .map_err(|error| error.to_string())?;
        c.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            format!("{APP_ID}\0{APP_ID}\0").as_bytes(),
        )
        .map_err(|error| error.to_string())?;
        c.change_property32(
            PropMode::REPLACE,
            win,
            atom("_NET_WM_WINDOW_TYPE")?,
            AtomEnum::ATOM,
            &[atom("_NET_WM_WINDOW_TYPE_DIALOG")?],
        )
        .map_err(|error| error.to_string())?;
        let mut hints = vec![0u32; 18];
        hints[0] = 1 << 4 | 1 << 5 | 1 << 10 | 1 << 11;
        hints[5] = u32::from(MIN_WIDTH);
        hints[6] = u32::from(MIN_HEIGHT);
        hints[7] = 1;
        hints[8] = 1;
        hints[9] = PAD as u32;
        hints[10] = PAD as u32;
        c.change_property32(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_NORMAL_HINTS,
            AtomEnum::WM_SIZE_HINTS,
            &hints,
        )
        .map_err(|error| error.to_string())?;
        c.change_property32(
            PropMode::REPLACE,
            win,
            atom("WM_PROTOCOLS")?,
            AtomEnum::ATOM,
            &[atom("WM_DELETE_WINDOW")?],
        )
        .map_err(|error| error.to_string())?;
        c.map_window(win).map_err(|error| error.to_string())?;
        c.sync().map_err(|error| error.to_string())?;
        Ok(Self {
            c,
            win,
            gc,
            back,
            w: WIDTH,
            h: HEIGHT,
            root,
            depth,
            image_lsb,
            palette,
            font,
            keys,
        })
    }

    fn resize_backing(&mut self) -> Result<(), String> {
        let pixmap = self.c.generate_id().map_err(|error| error.to_string())?;
        self.c
            .create_pixmap(self.depth, pixmap, self.root, self.w, self.h)
            .map_err(|error| error.to_string())?;
        let old = std::mem::replace(&mut self.back, pixmap);
        self.c.free_pixmap(old).map_err(|error| error.to_string())?;
        Ok(())
    }

    fn fill(&self, colour: u32, rect: Rectangle) -> Result<(), String> {
        if rect.width == 0 || rect.height == 0 {
            return Ok(());
        }
        self.c
            .change_gc(self.gc, &ChangeGCAux::new().foreground(colour))
            .map_err(|error| error.to_string())?;
        self.c
            .poly_fill_rectangle(self.back, self.gc, &[rect])
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| error.to_string())
    }

    fn upload_to(
        &self,
        target: Window,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        pixels: &[u8],
    ) -> Result<(), String> {
        let bgr = self.image_lsb;
        let mut data = Vec::with_capacity(pixels.len());
        for chunk in pixels.chunks_exact(4) {
            if bgr {
                data.extend([chunk[2], chunk[1], chunk[0], 255]);
            } else {
                data.extend([chunk[0], chunk[1], chunk[2], 255]);
            }
        }
        self.c
            .put_image(
                ImageFormat::Z_PIXMAP,
                target,
                self.gc,
                w,
                h,
                x,
                y,
                0,
                self.depth,
                data.as_slice(),
            )
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| error.to_string())
    }

    fn label_on(
        &self,
        x: i16,
        y: i16,
        text: &str,
        colour: u32,
        bg: u32,
        size: f32,
    ) -> Result<(), String> {
        let (w, h, pixels) = self.font.size(size).render(text, colour, bg);
        if w == 0 || h == 0 {
            return Ok(());
        }
        self.upload_to(self.back, x, y, w as u16, h as u16, &pixels)
    }

    fn label(&self, x: i16, y: i16, text: &str, colour: u32, size: f32) -> Result<(), String> {
        self.label_on(x, y, text, colour, self.palette.background, size)
    }

    fn image_fill(&self, rect: Rectangle, image: &RgbaImage) -> Result<(), String> {
        let (iw, ih) = (image.width(), image.height());
        if iw == 0 || ih == 0 || rect.width == 0 || rect.height == 0 {
            return Ok(());
        }
        let scale =
            (f64::from(rect.width) / f64::from(iw)).max(f64::from(rect.height) / f64::from(ih));
        let tw = (f64::from(iw) * scale).round().max(f64::from(rect.width)) as u32;
        let th = (f64::from(ih) * scale).round().max(f64::from(rect.height)) as u32;
        let scaled = image::imageops::resize(image, tw, th, image::imageops::FilterType::Triangle);
        let x0 = (tw - u32::from(rect.width)) / 2;
        let y0 = (th - u32::from(rect.height)) / 2;
        let mut pixels = Vec::with_capacity(usize::from(rect.width) * usize::from(rect.height) * 4);
        for y in y0..y0 + u32::from(rect.height) {
            for x in x0..x0 + u32::from(rect.width) {
                pixels.extend_from_slice(&scaled.get_pixel(x, y).0);
            }
        }
        self.upload_to(self.back, rect.x, rect.y, rect.width, rect.height, &pixels)
    }

    fn button(
        &self,
        rect: Rectangle,
        text: &str,
        active: bool,
        enabled: bool,
    ) -> Result<(), String> {
        let (bg, fg) = if !enabled {
            (self.palette.background, self.palette.border)
        } else if active {
            (self.palette.accent, self.palette.background)
        } else {
            (self.palette.surface, self.palette.foreground)
        };
        self.fill(bg, rect)?;
        self.fill(
            self.palette.border,
            Rectangle {
                width: rect.width,
                height: 1,
                ..rect
            },
        )?;
        let (_, text_h, _) = self.font.size(13.0).render(text, fg, bg);
        let y = rect.y + (i16::try_from(text_h).unwrap_or(0) - 8).max(0) / 2;
        self.label_on(rect.x + 8, y, text, fg, bg, 13.0)
    }

    fn ring(&self, rect: Rectangle) -> Result<(), String> {
        let outer = Rectangle {
            x: rect.x - 2,
            y: rect.y - 2,
            width: rect.width + 4,
            height: rect.height + 4,
        };
        for edge in [
            Rectangle {
                width: outer.width,
                height: 1,
                ..outer
            },
            Rectangle {
                y: outer.y + i16::try_from(outer.height).unwrap_or(0) - 1,
                width: outer.width,
                height: 1,
                ..outer
            },
            Rectangle {
                width: 1,
                height: outer.height,
                ..outer
            },
            Rectangle {
                x: outer.x + i16::try_from(outer.width).unwrap_or(0) - 1,
                width: 1,
                height: outer.height,
                ..outer
            },
        ] {
            self.fill(self.palette.foreground, edge)?;
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), String> {
        self.c
            .copy_area(self.back, self.win, self.gc, 0, 0, 0, 0, self.w, self.h)
            .map_err(|error| error.to_string())?;
        self.c.flush().map_err(|error| error.to_string())
    }

    fn paint(&self, state: &State) -> Result<(), String> {
        let l = layout(self.w, self.h);
        self.fill(
            self.palette.background,
            Rectangle {
                x: 0,
                y: 0,
                width: self.w,
                height: self.h,
            },
        )?;
        self.label(PAD, PAD - 8, APP_NAME, self.palette.foreground, 20.0)?;
        let appearance = if state.catalog.appearance_overrides {
            " · appearance overrides active"
        } else {
            ""
        };
        let subtitle = format!(
            "{} themes · active {}{}",
            state
                .cards
                .iter()
                .filter(|card| state.show_hidden || !card.entry.hidden)
                .count(),
            state.catalog.active,
            appearance
        );
        self.label(
            PAD,
            PAD + 22,
            &truncate(&subtitle, 100),
            self.palette.muted,
            12.0,
        )?;
        self.paint_cards(state, l)?;
        self.paint_panel(state, l)?;
        self.paint_actions(state, l)?;
        let colour = if state.message_is_error {
            state
                .selected_card()
                .map(|card| card.palette.accent)
                .unwrap_or(self.palette.accent)
        } else {
            self.palette.muted
        };
        self.label(
            l.footer.x,
            l.footer.y,
            &truncate(&state.message, (self.w as usize / 7).saturating_sub(2)),
            colour,
            12.0,
        )?;
        self.label(
            l.footer.x,
            l.footer.y + 20,
            "Tab focus  arrows choose  Enter apply/activate  I import  H hide  D remove  V hidden  A apply  F1 help  Esc close",
            self.palette.muted,
            11.0,
        )?;
        if let Some(focus) = state.focus {
            if let Some(rect) = focus_rect(state, l, focus) {
                self.ring(rect)?;
            }
        }
        if let Some(modal) = &state.modal {
            self.paint_modal(state, modal)?;
        }
        Ok(())
    }

    fn paint_cards(&self, state: &State, l: Layout) -> Result<(), String> {
        let visible = state.visible_indices();
        if visible.is_empty() {
            self.label(
                l.grid.x + 20,
                l.grid.y + 30,
                "No themes are visible. Press V to show hidden themes.",
                self.palette.muted,
                13.0,
            )?;
            return Ok(());
        }
        for (position, card_index) in visible.iter().enumerate() {
            let row = position / l.cols;
            if row < state.scroll || row >= state.scroll + l.rows {
                continue;
            }
            let screen_index = position - state.scroll * l.cols;
            let rect = card_rect(l, screen_index);
            let card = &state.cards[*card_index];
            self.paint_card(state, l, rect, *card_index, card)?;
        }
        Ok(())
    }

    fn paint_card(
        &self,
        state: &State,
        _l: Layout,
        rect: Rectangle,
        _card_index: usize,
        card: &Card,
    ) -> Result<(), String> {
        let active = card.entry.id == state.catalog.active;
        let selected = state.selected.as_deref() == Some(card.entry.id.as_str());
        self.fill(card.palette.background, rect)?;
        if let Some(path) = &card.wallpaper {
            if let Some(Some(image)) = state.thumbs.get(path) {
                self.image_fill(
                    Rectangle {
                        x: rect.x + 1,
                        y: rect.y + 1,
                        width: rect.width.saturating_sub(2),
                        height: rect.height.saturating_sub(50),
                    },
                    image,
                )?;
            }
        }
        // A small bar and window frame make the card read as a desktop theme,
        // not just another wallpaper thumbnail.
        let bar = Rectangle {
            x: rect.x + 8,
            y: rect.y + 8,
            width: rect.width.saturating_sub(16),
            height: 22,
        };
        self.fill(card.palette.surface, bar)?;
        self.fill(
            card.palette.accent,
            Rectangle {
                width: 38,
                height: 22,
                ..bar
            },
        )?;
        self.label_on(
            bar.x + 8,
            bar.y + 4,
            "NIRU",
            card.palette.foreground,
            card.palette.accent,
            10.0,
        )?;
        self.fill(
            card.palette.border,
            Rectangle {
                x: rect.x + rect.width as i16 / 3,
                y: rect.y + 44,
                width: rect.width / 2,
                height: 54,
            },
        )?;
        self.fill(
            card.palette.surface,
            Rectangle {
                x: rect.x + rect.width as i16 / 3 + 2,
                y: rect.y + 46,
                width: rect.width / 2 - 4,
                height: 50,
            },
        )?;
        self.fill(
            card.palette.accent,
            Rectangle {
                x: rect.x + rect.width as i16 / 3 + 2,
                y: rect.y + 46,
                width: rect.width / 2 - 4,
                height: 3,
            },
        )?;
        let footer = Rectangle {
            x: rect.x,
            y: rect.y + rect.height as i16 - 50,
            width: rect.width,
            height: 50,
        };
        self.fill(card.palette.background, footer)?;
        let name = truncate(&card.entry.name, 28);
        self.label_on(
            footer.x + 8,
            footer.y + 4,
            &name,
            card.palette.foreground,
            card.palette.background,
            12.0,
        )?;
        let origin = if card.entry.overridden {
            "built-in override"
        } else if card.entry.builtin {
            "built-in"
        } else {
            "user"
        };
        let flags = format!(
            "{}{}{}",
            origin,
            if card.entry.hidden { " · hidden" } else { "" },
            if card.entry.valid { "" } else { " · invalid" }
        );
        self.label_on(
            footer.x + 8,
            footer.y + 20,
            &truncate(&flags, 34),
            card.palette.muted,
            card.palette.background,
            10.0,
        )?;
        for (index, colour) in [
            card.palette.background,
            card.palette.foreground,
            card.palette.muted,
            card.palette.border,
            card.palette.accent,
        ]
        .into_iter()
        .enumerate()
        {
            self.fill(
                colour,
                Rectangle {
                    x: footer.x + 8 + index as i16 * 18,
                    y: footer.y + 34,
                    width: 14,
                    height: 10,
                },
            )?;
        }
        if !card.entry.valid {
            if let Some(error) = &card.entry.error {
                self.label_on(
                    footer.x + 8,
                    footer.y + 34,
                    &truncate(error, 36),
                    card.palette.accent,
                    card.palette.background,
                    9.0,
                )?;
            }
        }
        if active {
            self.fill(
                card.palette.accent,
                Rectangle {
                    width: rect.width,
                    height: 2,
                    ..rect
                },
            )?;
            self.label_on(
                footer.x + footer.width as i16 - 72,
                footer.y + 4,
                "ACTIVE",
                card.palette.accent,
                card.palette.background,
                9.0,
            )?;
        }
        if selected {
            self.ring(rect)?;
        }
        Ok(())
    }

    fn paint_panel(&self, state: &State, l: Layout) -> Result<(), String> {
        self.fill(self.palette.surface, l.panel)?;
        self.label(
            l.panel.x + 10,
            l.panel.y + 8,
            "Preview",
            self.palette.muted,
            12.0,
        )?;
        let Some(card) = state.selected_card() else {
            self.label(
                l.panel.x + 12,
                l.panel.y + 40,
                "Select a theme.",
                self.palette.muted,
                13.0,
            )?;
            return Ok(());
        };
        let palette = card.palette;
        let preview = Rectangle {
            x: l.panel.x + 10,
            y: l.panel.y + 30,
            width: l.panel.width.saturating_sub(20),
            height: 206,
        };
        self.fill(palette.background, preview)?;
        if let Some(path) = &card.wallpaper {
            if let Some(Some(image)) = state.thumbs.get(path) {
                self.image_fill(preview, image)?;
            }
        }
        let bar = Rectangle {
            x: preview.x,
            y: preview.y,
            width: preview.width,
            height: 24,
        };
        self.fill(palette.surface, bar)?;
        for index in 0..8 {
            self.fill(
                if index == 0 {
                    palette.accent
                } else {
                    palette.border
                },
                Rectangle {
                    x: bar.x + 8 + index * 14,
                    y: bar.y + 8,
                    width: 9,
                    height: 8,
                },
            )?;
        }
        self.label_on(
            bar.x + bar.width as i16 - 66,
            bar.y + 4,
            "NIRU",
            palette.foreground,
            palette.surface,
            10.0,
        )?;
        let window = Rectangle {
            x: preview.x + 36,
            y: preview.y + 54,
            width: preview.width.saturating_sub(72),
            height: 82,
        };
        self.fill(palette.surface, window)?;
        self.fill(
            palette.accent,
            Rectangle {
                width: window.width,
                height: 3,
                ..window
            },
        )?;
        self.label_on(
            window.x + 8,
            window.y + 12,
            "Focused window",
            palette.foreground,
            palette.surface,
            12.0,
        )?;
        self.label_on(
            window.x + 8,
            window.y + 32,
            "A calm, keyboard-first desktop",
            palette.muted,
            palette.surface,
            10.0,
        )?;
        self.fill(
            palette.background,
            Rectangle {
                x: window.x + 8,
                y: window.y + 50,
                width: window.width.saturating_sub(16),
                height: 22,
            },
        )?;
        self.label_on(
            window.x + 12,
            window.y + 54,
            "launcher sample",
            palette.foreground,
            palette.background,
            9.0,
        )?;
        self.fill(
            palette.accent,
            Rectangle {
                width: 5,
                height: 22,
                x: window.x + 8,
                y: window.y + 50,
            },
        )?;
        let swatches_y = preview.y + preview.height as i16 + 14;
        for (index, colour) in [
            palette.background,
            palette.foreground,
            palette.muted,
            palette.border,
            palette.accent,
        ]
        .into_iter()
        .enumerate()
        {
            self.fill(
                colour,
                Rectangle {
                    x: l.panel.x + 12 + index as i16 * 32,
                    y: swatches_y,
                    width: 26,
                    height: 20,
                },
            )?;
        }
        let active = card.entry.id == state.catalog.active;
        let origin = if card.entry.overridden {
            "built-in override"
        } else if card.entry.builtin {
            "built-in"
        } else {
            "user"
        };
        let mut y = swatches_y + 34;
        self.label(
            l.panel.x + 12,
            y,
            &truncate(&card.entry.name, 34),
            self.palette.foreground,
            14.0,
        )?;
        y += 22;
        self.label(
            l.panel.x + 12,
            y,
            &format!("{} · {origin}", card.entry.id),
            self.palette.muted,
            10.0,
        )?;
        y += 18;
        self.label(
            l.panel.x + 12,
            y,
            if active {
                "Active theme"
            } else {
                "Not applied"
            },
            if active {
                palette.accent
            } else {
                self.palette.muted
            },
            11.0,
        )?;
        y += 18;
        let wallpaper = if card.entry.wallpaper_available {
            "Wallpaper available"
        } else {
            "No wallpaper · theme colour fallback"
        };
        self.label(l.panel.x + 12, y, wallpaper, self.palette.muted, 10.0)?;
        y += 18;
        if card.entry.hidden {
            self.label(
                l.panel.x + 12,
                y,
                "Hidden from the normal grid",
                self.palette.muted,
                10.0,
            )?;
            y += 18;
        }
        if !card.entry.valid {
            self.label(
                l.panel.x + 12,
                y,
                &truncate(card.entry.error.as_deref().unwrap_or("invalid theme"), 38),
                palette.accent,
                10.0,
            )?;
            y += 18;
        }
        if state.catalog.appearance_overrides && active {
            self.label(
                l.panel.x + 12,
                y,
                "appearance.toml colour overrides apply",
                self.palette.muted,
                10.0,
            )?;
        }
        Ok(())
    }

    fn paint_actions(&self, state: &State, l: Layout) -> Result<(), String> {
        let card = state.selected_card();
        let active = card
            .map(|card| card.entry.id == state.catalog.active)
            .unwrap_or(false);
        let builtin = card
            .map(|card| card.entry.builtin && !card.entry.overridden)
            .unwrap_or(false);
        let hidden = card.map(|card| card.entry.hidden).unwrap_or(false);
        let valid = card.map(|card| card.entry.valid).unwrap_or(false);
        self.button(action_rect(l, 0), "Apply", active, valid)?;
        self.button(action_rect(l, 1), "Import theme…", false, true)?;
        self.button(
            action_rect(l, 2),
            "Remove",
            false,
            card.is_some() && !builtin,
        )?;
        self.button(
            action_rect(l, 3),
            if hidden { "Show theme" } else { "Hide theme" },
            false,
            card.is_some(),
        )?;
        self.button(
            action_rect(l, 4),
            if state.show_hidden {
                "Hidden: shown"
            } else {
                "Show hidden"
            },
            state.show_hidden,
            true,
        )?;
        self.button(action_rect(l, 5), "Reload", false, true)?;
        Ok(())
    }

    fn paint_modal(&self, state: &State, modal: &Modal) -> Result<(), String> {
        let rect = modal_rect(self.w, self.h);
        self.fill(self.palette.background, rect)?;
        self.fill(
            self.palette.accent,
            Rectangle {
                width: rect.width,
                height: 2,
                ..rect
            },
        )?;
        match modal {
            Modal::Import {
                theme,
                wallpaper,
                id,
                field,
                replace,
            } => {
                self.label(
                    rect.x + 24,
                    rect.y + 18,
                    "Import theme",
                    self.palette.foreground,
                    16.0,
                )?;
                self.label(
                    rect.x + 24,
                    rect.y + 48,
                    "Theme TOML path",
                    self.palette.muted,
                    11.0,
                )?;
                self.import_field(rect, 0, theme, *field == ImportField::Theme)?;
                self.label(
                    rect.x + 24,
                    rect.y + 108,
                    "Optional PNG wallpaper",
                    self.palette.muted,
                    11.0,
                )?;
                self.import_field(rect, 1, wallpaper, *field == ImportField::Wallpaper)?;
                self.label(
                    rect.x + 24,
                    rect.y + 168,
                    "Optional theme id",
                    self.palette.muted,
                    11.0,
                )?;
                self.import_field(rect, 2, id, *field == ImportField::Id)?;
                self.label(
                    rect.x + 24,
                    rect.y + 228,
                    &format!(
                        "Ctrl+R replace existing user theme: {}   Tab next field   Enter import   Esc cancel",
                        if *replace { "on" } else { "off" }
                    ),
                    self.palette.muted,
                    10.0,
                )?;
            }
            Modal::ConfirmRemove { id, name } => {
                self.label(
                    rect.x + 24,
                    rect.y + 30,
                    "Remove theme?",
                    self.palette.foreground,
                    18.0,
                )?;
                self.label(
                    rect.x + 24,
                    rect.y + 68,
                    &format!("Remove {name} ({id})?"),
                    self.palette.muted,
                    13.0,
                )?;
                self.label(
                    rect.x + 24,
                    rect.y + 96,
                    "Only this user theme and a recorded imported wallpaper are removed.",
                    self.palette.muted,
                    11.0,
                )?;
                self.label(
                    rect.x + 24,
                    rect.y + 132,
                    "Enter confirms. Escape keeps the theme.",
                    self.palette.muted,
                    11.0,
                )?;
                self.button(
                    Rectangle {
                        x: rect.x + 24,
                        y: rect.y + 170,
                        width: 170,
                        height: 30,
                    },
                    "Remove theme",
                    true,
                    true,
                )?;
            }
            Modal::Help => {
                self.label(
                    rect.x + 24,
                    rect.y + 20,
                    "NIRU Theme Selector",
                    self.palette.foreground,
                    18.0,
                )?;
                for (index, line) in [
                    "Tab / Shift+Tab     move between cards and actions",
                    "Arrows              choose a theme card",
                    "Enter / Space       apply or activate the focused control",
                    "A                   apply the selected theme",
                    "I                   import a theme TOML and optional PNG wallpaper",
                    "H                   hide/show the selected theme",
                    "D                   remove a user theme",
                    "V                   show or hide hidden themes",
                    "R                   reload the catalogue",
                    "Esc                 close a dialog, then the window",
                    "The preview is representative; Apply uses the existing NIRUWM theme path.",
                ]
                .into_iter()
                .enumerate()
                {
                    self.label(
                        rect.x + 24,
                        rect.y + 60 + index as i16 * 20,
                        line,
                        self.palette.muted,
                        11.0,
                    )?;
                }
            }
        }
        let _ = state;
        Ok(())
    }

    fn import_field(
        &self,
        modal: Rectangle,
        index: usize,
        text: &str,
        focused: bool,
    ) -> Result<(), String> {
        let rect = field_rect(modal, index);
        self.fill(
            if focused {
                self.palette.surface
            } else {
                self.palette.background
            },
            rect,
        )?;
        self.fill(
            if focused {
                self.palette.accent
            } else {
                self.palette.border
            },
            Rectangle {
                width: rect.width,
                height: 1,
                ..rect
            },
        )?;
        let shown = if text.is_empty() {
            "…".to_string()
        } else {
            format!("{text}_")
        };
        self.label_on(
            rect.x + 6,
            rect.y + 7,
            &truncate(&shown, 84),
            self.palette.foreground,
            if focused {
                self.palette.surface
            } else {
                self.palette.background
            },
            11.0,
        )?;
        if focused {
            self.ring(rect)?;
        }
        Ok(())
    }
}

pub fn run(app: &mut App, mut state: State) -> Result<(), String> {
    let mut dirty = true;
    let mut last_refresh = Instant::now();
    let mut last_activity = Instant::now();
    'outer: loop {
        let mut handled = false;
        while let Some(event) = app.c.poll_for_event().map_err(|error| error.to_string())? {
            handled = true;
            last_activity = Instant::now();
            if !matches!(event, Event::NoExposure(_)) {
                dirty = true;
            }
            match event {
                Event::ConfigureNotify(event) => {
                    if event.width != app.w || event.height != app.h {
                        app.w = event.width.max(MIN_WIDTH);
                        app.h = event.height.max(MIN_HEIGHT);
                        app.resize_backing()?;
                    }
                }
                Event::KeyPress(event) => {
                    let shift = event.state.contains(KeyButMask::SHIFT);
                    let ctrl = event.state.contains(KeyButMask::CONTROL);
                    let key = app.keys.sym(event.detail, shift);
                    if let Some(modal) = state.modal.clone() {
                        match modal {
                            Modal::Import {
                                mut theme,
                                mut wallpaper,
                                mut id,
                                mut field,
                                mut replace,
                            } => match key {
                                K_ESCAPE => state.modal = None,
                                K_TAB => {
                                    field = match field {
                                        ImportField::Theme => ImportField::Wallpaper,
                                        ImportField::Wallpaper => ImportField::Id,
                                        ImportField::Id => ImportField::Theme,
                                    };
                                    if shift {
                                        field = match field {
                                            ImportField::Theme => ImportField::Id,
                                            ImportField::Wallpaper => ImportField::Theme,
                                            ImportField::Id => ImportField::Wallpaper,
                                        };
                                    }
                                    state.modal = Some(Modal::Import {
                                        theme,
                                        wallpaper,
                                        id,
                                        field,
                                        replace,
                                    });
                                }
                                K_RETURN => {
                                    let modal = state.modal.clone().unwrap();
                                    match state.import(&modal) {
                                        Ok(_) => {
                                            state.modal = None;
                                            state.focus = None;
                                        }
                                        Err(error) => state.set_message(error, true),
                                    }
                                }
                                K_R if ctrl => {
                                    replace = !replace;
                                    state.modal = Some(Modal::Import {
                                        theme,
                                        wallpaper,
                                        id,
                                        field,
                                        replace,
                                    });
                                }
                                K_BACKSPACE => {
                                    match field {
                                        ImportField::Theme => {
                                            theme.pop();
                                        }
                                        ImportField::Wallpaper => {
                                            wallpaper.pop();
                                        }
                                        ImportField::Id => {
                                            id.pop();
                                        }
                                    }
                                    state.modal = Some(Modal::Import {
                                        theme,
                                        wallpaper,
                                        id,
                                        field,
                                        replace,
                                    });
                                }
                                _ => {
                                    if let Some(ch) = text_key(key) {
                                        let target = match field {
                                            ImportField::Theme => &mut theme,
                                            ImportField::Wallpaper => &mut wallpaper,
                                            ImportField::Id => &mut id,
                                        };
                                        if target.chars().count() < 255 {
                                            target.push(ch);
                                        }
                                    }
                                    state.modal = Some(Modal::Import {
                                        theme,
                                        wallpaper,
                                        id,
                                        field,
                                        replace,
                                    });
                                }
                            },
                            Modal::ConfirmRemove { .. } => match key {
                                K_ESCAPE => state.modal = None,
                                K_RETURN => {
                                    state.modal = None;
                                    if let Err(error) = state.remove_selected() {
                                        state.set_message(error, true);
                                    }
                                }
                                _ => {}
                            },
                            Modal::Help => {
                                let _ = &state;
                                state.modal = None;
                            }
                        }
                        continue;
                    }
                    match key {
                        K_ESCAPE | K_Q | K_Q_SHIFTED => break 'outer,
                        K_F1 => state.modal = Some(Modal::Help),
                        K_TAB => {
                            state.focus = next_focus(&state, state.focus, shift);
                            if let Some(Hit::Card(index)) = state.focus {
                                if let Some(card) = state.cards.get(index) {
                                    state.selected = Some(card.entry.id.clone());
                                }
                            }
                        }
                        K_LEFT | K_RIGHT | K_UP | K_DOWN => {
                            let l = layout(app.w, app.h);
                            let step = if key == K_LEFT || key == K_RIGHT {
                                1
                            } else {
                                l.cols
                            };
                            let delta = if key == K_LEFT || key == K_UP {
                                -(step as i32)
                            } else {
                                step as i32
                            };
                            move_selection(&mut state, delta, l);
                            state.focus = state
                                .selected
                                .as_deref()
                                .and_then(|id| {
                                    state.cards.iter().position(|card| card.entry.id == id)
                                })
                                .map(Hit::Card);
                        }
                        K_RETURN | K_SPACE => {
                            if let Some(focus) = state.focus {
                                activate(app, &mut state, focus)?;
                            } else if key == K_RETURN {
                                if let Err(error) = state.apply() {
                                    state.set_message(error, true);
                                }
                            }
                        }
                        K_A => {
                            if let Err(error) = state.apply() {
                                state.set_message(error, true);
                            }
                        }
                        K_I => {
                            state.modal = Some(Modal::Import {
                                theme: String::new(),
                                wallpaper: String::new(),
                                id: String::new(),
                                field: ImportField::Theme,
                                replace: false,
                            });
                            state.focus = None;
                        }
                        K_R => match state.refresh() {
                            Ok(_) => state.set_message("Reloaded the theme catalogue.", false),
                            Err(error) => state.set_message(error, true),
                        },
                        K_H => {
                            if let Err(error) = state.hide_toggle() {
                                state.set_message(error, true);
                            }
                        }
                        K_D => {
                            if let Some(card) = state.selected_card() {
                                if card.entry.builtin && !card.entry.overridden {
                                    state.set_message(
                                        "Bundled themes are hidden, not removed.",
                                        true,
                                    );
                                } else {
                                    state.modal = Some(Modal::ConfirmRemove {
                                        id: card.entry.id.clone(),
                                        name: card.entry.name.clone(),
                                    });
                                }
                            }
                        }
                        K_V => {
                            state.show_hidden = !state.show_hidden;
                            match state.refresh() {
                                Ok(_) => {}
                                Err(error) => state.set_message(error, true),
                            }
                        }
                        _ => {}
                    }
                }
                Event::ButtonPress(event) => {
                    let l = layout(app.w, app.h);
                    match event.detail {
                        4 => {
                            state.scroll = state.scroll.saturating_sub(1);
                            state.request_visible();
                        }
                        5 => {
                            state.scroll += 1;
                            state.request_visible();
                        }
                        _ => {
                            if let Some(hit) =
                                hit(&state, l, app.w, app.h, event.event_x, event.event_y)
                            {
                                match hit {
                                    Hit::Card(index) => {
                                        if let Some(card) = state.cards.get(index) {
                                            state.select(card.entry.id.clone());
                                        }
                                        state.focus = Some(Hit::Card(index));
                                    }
                                    Hit::ImportField(field) => {
                                        if let Some(Modal::Import { field: current, .. }) =
                                            state.modal.as_mut()
                                        {
                                            *current = field;
                                        }
                                    }
                                    _ => activate(app, &mut state, hit)?,
                                }
                            }
                        }
                    }
                }
                Event::ClientMessage(event) => {
                    let close = app
                        .c
                        .intern_atom(false, b"WM_PROTOCOLS")
                        .ok()
                        .and_then(|cookie| cookie.reply().ok())
                        .map(|reply| reply.atom);
                    if Some(event.type_) == close {
                        break 'outer;
                    }
                }
                _ => {}
            }
        }
        if state.collect_thumbs() {
            dirty = true;
        }
        let interval = Duration::from_secs(5);
        if last_refresh.elapsed() >= interval
            && last_activity.elapsed() >= Duration::from_millis(200)
        {
            last_refresh = Instant::now();
            match state.refresh() {
                Ok(true) => dirty = true,
                Ok(false) => {}
                Err(error) => state.set_message(error, true),
            }
            state.request_visible();
        }
        if dirty {
            app.palette = state.palette;
            app.paint(&state)?;
            app.flush()?;
            dirty = false;
        } else {
            app.c.flush().map_err(|error| error.to_string())?;
        }
        if !handled {
            wait_for_input(app)?;
        }
        let l = layout(app.w, app.h);
        state.ensure_visible(l.cols, l.rows);
        state.request_visible();
    }
    Ok(())
}

fn next_focus(state: &State, current: Option<Hit>, backward: bool) -> Option<Hit> {
    let order = focus_order(state);
    if order.is_empty() {
        return None;
    }
    let index = current.and_then(|hit| order.iter().position(|candidate| *candidate == hit));
    let next = match (index, backward) {
        (Some(index), true) => (index + order.len() - 1) % order.len(),
        (Some(index), false) => (index + 1) % order.len(),
        (None, true) => order.len() - 1,
        (None, false) => 0,
    };
    order.get(next).copied()
}

fn focus_order(state: &State) -> Vec<Hit> {
    let mut order: Vec<Hit> = state.visible_indices().into_iter().map(Hit::Card).collect();
    order.extend([
        Hit::Apply,
        Hit::Import,
        Hit::Remove,
        Hit::Hide,
        Hit::ToggleHidden,
        Hit::Reload,
    ]);
    order
}

fn focus_rect(state: &State, l: Layout, focus: Hit) -> Option<Rectangle> {
    match focus {
        Hit::Card(index) => {
            let position = state.visible_position(&state.cards.get(index)?.entry.id)?;
            if position < state.scroll * l.cols {
                return None;
            }
            let screen_index = position - state.scroll * l.cols;
            if screen_index / l.cols >= l.rows {
                return None;
            }
            Some(card_rect(l, screen_index))
        }
        Hit::Apply => Some(action_rect(l, 0)),
        Hit::Import => Some(action_rect(l, 1)),
        Hit::Remove => Some(action_rect(l, 2)),
        Hit::Hide => Some(action_rect(l, 3)),
        Hit::ToggleHidden => Some(action_rect(l, 4)),
        Hit::Reload => Some(action_rect(l, 5)),
        Hit::ImportField(_) | Hit::ImportReplace | Hit::Confirm => None,
    }
}

fn move_selection(state: &mut State, delta: i32, l: Layout) {
    let visible = state.visible_indices();
    if visible.is_empty() {
        return;
    }
    let selected_position = state
        .selected
        .as_deref()
        .and_then(|id| {
            visible
                .iter()
                .position(|index| state.cards[*index].entry.id == id)
        })
        .unwrap_or(0) as i32;
    let next = (selected_position + delta).rem_euclid(visible.len() as i32) as usize;
    if let Some(index) = visible.get(next) {
        state.selected = Some(state.cards[*index].entry.id.clone());
    }
    state.ensure_visible(l.cols, l.rows);
}

fn activate(app: &mut App, state: &mut State, hit: Hit) -> Result<(), String> {
    match hit {
        Hit::Card(index) => {
            if let Some(card) = state.cards.get(index) {
                state.select(card.entry.id.clone());
                state.focus = Some(Hit::Card(index));
            }
        }
        Hit::Apply => {
            if let Err(error) = state.apply() {
                state.set_message(error, true);
            }
        }
        Hit::Import => {
            state.modal = Some(Modal::Import {
                theme: String::new(),
                wallpaper: String::new(),
                id: String::new(),
                field: ImportField::Theme,
                replace: false,
            });
            state.focus = None;
        }
        Hit::Remove => {
            if let Some(card) = state.selected_card() {
                if card.entry.builtin && !card.entry.overridden {
                    state.set_message("Bundled themes are hidden, not removed.", true);
                } else {
                    state.modal = Some(Modal::ConfirmRemove {
                        id: card.entry.id.clone(),
                        name: card.entry.name.clone(),
                    });
                }
            }
        }
        Hit::Hide => {
            if let Err(error) = state.hide_toggle() {
                state.set_message(error, true);
            }
        }
        Hit::ToggleHidden => {
            state.show_hidden = !state.show_hidden;
            match state.refresh() {
                Ok(_) => {}
                Err(error) => state.set_message(error, true),
            }
        }
        Hit::Reload => match state.refresh() {
            Ok(_) => state.set_message("Reloaded the theme catalogue.", false),
            Err(error) => state.set_message(error, true),
        },
        Hit::ImportField(field) => {
            if let Some(Modal::Import { field: current, .. }) = state.modal.as_mut() {
                *current = field;
            }
        }
        Hit::ImportReplace => {
            if let Some(Modal::Import { replace, .. }) = state.modal.as_mut() {
                *replace = !*replace;
            }
        }
        Hit::Confirm => {
            state.modal = None;
            if let Err(error) = state.remove_selected() {
                state.set_message(error, true);
            }
        }
    }
    let _ = app;
    Ok(())
}

pub struct Keys {
    table: Vec<Vec<u32>>,
    min: u8,
}

impl Keys {
    pub fn new(c: &RustConnection) -> Result<Self, String> {
        let setup = c.setup();
        let count = setup.max_keycode - setup.min_keycode + 1;
        let map = c
            .get_keyboard_mapping(setup.min_keycode, count)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        let per = map.keysyms_per_keycode.max(1) as usize;
        let table = map
            .keysyms
            .chunks(per)
            .map(|syms| syms.iter().copied().take(2).collect())
            .collect();
        Ok(Self {
            table,
            min: setup.min_keycode,
        })
    }

    pub fn sym(&self, code: u8, shift: bool) -> u32 {
        code.checked_sub(self.min)
            .and_then(|index| self.table.get(index as usize))
            .map(|levels| {
                levels
                    .get(usize::from(shift))
                    .or_else(|| levels.first())
                    .copied()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }
}

fn text_key(sym: u32) -> Option<char> {
    if (0x20..=0x7e).contains(&sym) {
        return char::from_u32(sym);
    }
    match sym {
        0x00e5 => Some('å'),
        0x00c5 => Some('Å'),
        0x00e4 => Some('ä'),
        0x00c4 => Some('Ä'),
        0x00f6 => Some('ö'),
        0x00d6 => Some('Ö'),
        _ => None,
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn wait_for_input(app: &App) -> Result<(), String> {
    use std::os::unix::io::AsRawFd;
    let fd = app.c.stream().as_raw_fd();
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd describing the live X socket is passed.
    let _ = unsafe { libc::poll(&mut poll, 1, 250) };
    Ok(())
}

fn spawn_workers(work: Receiver<PathBuf>, done: Sender<(PathBuf, Option<RgbaImage>)>) {
    use std::sync::{Arc, Mutex};
    let work = Arc::new(Mutex::new(work));
    for _ in 0..2 {
        let work = work.clone();
        let done = done.clone();
        std::thread::spawn(move || loop {
            let path = {
                let Ok(receiver) = work.lock() else {
                    return;
                };
                match receiver.recv() {
                    Ok(path) => path,
                    Err(_) => return,
                }
            };
            let image = thumbs::decode_preview(&path);
            if done.send((path, image)).is_err() {
                return;
            }
        });
    }
}
