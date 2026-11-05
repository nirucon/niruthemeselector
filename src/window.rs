//! The NIRU Theme Selector window.
//!
//! A native immediate-mode X11 window. The catalogue, application and import
//! rules live in niructl; this file presents them, keeps a local preview
//! selection, and runs the event loop.
//!
//! Event model: the loop sleeps in `poll` on the X connection, an inotify
//! watch on the files `niructl` changes, and a wake-up pipe the thumbnail
//! workers and the action worker write to. It has a timeout only while
//! something is pending (a debounced refresh, the Undo toast, a clipboard
//! answer, a running action's backstop). An idle window starts no process.

use crate::{
    backend::{self, Action, Catalog},
    import::{self, PathStatus, Want},
    keyboard::Keys,
    layout::{self, contains, layout, Button, Layout, Mode, BUTTONS, CHIPS, PAD},
    state::{Browse, Card, Command, Hit, ImportForm, Key, Modal, State, DOUBLE_CLICK_MS},
    text,
    theme::{self, Palette},
    thumbs,
    watch::{self, Debounce, Wake, Watcher},
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc,
    },
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
const PASTE_DEADLINE: Duration = Duration::from_secs(1);
const SCALED_CACHE: usize = 96;
const BROWSE_ROW: i16 = 22;

type Scaled = HashMap<(PathBuf, u16, u16), (String, Vec<u8>)>;
type WorkerResult = (u64, Result<(), String>, Option<Result<Catalog, String>>);

struct Atoms {
    protocols: Atom,
    delete: Atom,
    clipboard: Atom,
    utf8: Atom,
    incr: Atom,
    paste: Atom,
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
    palette: Palette,
    font: text::Font,
    keys: Keys,
    atoms: Atoms,
    wake: Arc<Wake>,
    watcher: Watcher,
    debounce: Debounce,
    pool: thumbs::Pool,
    cache: thumbs::Cache,
    jobs: Sender<(u64, Action)>,
    results: Receiver<WorkerResult>,
    /// Scaled, X-ordered pixels per (wallpaper, size), so a hover repaint
    /// does not resize every thumbnail again.
    scaled: RefCell<Scaled>,
    /// Where the Undo link was painted, for hit testing.
    undo_rect: Cell<Option<Rectangle>>,
    paste_until: Option<Instant>,
    last_click: Option<(Hit, u32)>,
    /// While the scrollbar is dragged: where in the thumb it was taken.
    drag: Option<i16>,
    /// The last pointer position inside the window, so hover follows a
    /// change the keyboard made (a filter, a scroll) without a motion event.
    pointer: Option<(i16, i16)>,
}

fn x<E: std::fmt::Display>(error: E) -> String {
    error.to_string()
}

impl App {
    pub fn open() -> Result<Self, String> {
        let (c, screen) = x11rb::connect(None).map_err(x)?;
        let root = c.setup().roots[screen].root;
        let depth = c.get_geometry(root).map_err(x)?.reply().map_err(x)?.depth;
        if ![24, 32].contains(&depth) {
            return Err(format!(
                "NIRU Theme Selector needs a 24 or 32-bit screen, this one is {depth}"
            ));
        }
        let image_lsb = c.setup().image_byte_order == ImageOrder::LSB_FIRST;
        let palette = theme::load();
        let font = text::Font::load(palette);
        let keys = Keys::load(&c)?;
        let atom = |name: &str| -> Result<Atom, String> {
            c.intern_atom(false, name.as_bytes())
                .map_err(x)?
                .reply()
                .map(|reply| reply.atom)
                .map_err(x)
        };
        // Interned once: the close handler compares, it never asks again.
        let atoms = Atoms {
            protocols: atom("WM_PROTOCOLS")?,
            delete: atom("WM_DELETE_WINDOW")?,
            clipboard: atom("CLIPBOARD")?,
            utf8: atom("UTF8_STRING")?,
            incr: atom("INCR")?,
            paste: atom("NIRUTHEMESELECTOR_PASTE")?,
        };
        let win = c.generate_id().map_err(x)?;
        let gc = c.generate_id().map_err(x)?;
        let back = c.generate_id().map_err(x)?;
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
                        | EventMask::BUTTON_RELEASE
                        | EventMask::POINTER_MOTION
                        | EventMask::LEAVE_WINDOW
                        | EventMask::STRUCTURE_NOTIFY
                        | EventMask::FOCUS_CHANGE,
                ),
        )
        .map_err(x)?;
        c.create_gc(gc, win, &CreateGCAux::new().graphics_exposures(0))
            .map_err(x)?;
        c.create_pixmap(depth, back, root, WIDTH, HEIGHT)
            .map_err(x)?;
        c.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            APP_NAME.as_bytes(),
        )
        .map_err(x)?;
        c.change_property8(
            PropMode::REPLACE,
            win,
            atom("_NET_WM_NAME")?,
            atoms.utf8,
            APP_NAME.as_bytes(),
        )
        .map_err(x)?;
        c.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            format!("{APP_ID}\0{APP_ID}\0").as_bytes(),
        )
        .map_err(x)?;
        // NIRUWM floats dialogs; tiled smaller anyway, the window paints a
        // compact layout for the real size.
        c.change_property32(
            PropMode::REPLACE,
            win,
            atom("_NET_WM_WINDOW_TYPE")?,
            AtomEnum::ATOM,
            &[atom("_NET_WM_WINDOW_TYPE_DIALOG")?],
        )
        .map_err(x)?;
        let mut hints = vec![0u32; 18];
        hints[0] = 1 << 4 | 1 << 5 | 1 << 10 | 1 << 11;
        hints[5] = u32::from(layout::COMPACT_W);
        hints[6] = u32::from(layout::COMPACT_H);
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
        .map_err(x)?;
        c.change_property32(
            PropMode::REPLACE,
            win,
            atoms.protocols,
            AtomEnum::ATOM,
            &[atoms.delete],
        )
        .map_err(x)?;
        c.map_window(win).map_err(x)?;
        c.sync().map_err(x)?;
        let wake = Arc::new(Wake::new()?);
        let watcher = Watcher::new()?;
        let pool = thumbs::Pool::new(wake.clone());
        let (jobs, results) = spawn_worker(wake.clone());
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
            atoms,
            wake,
            watcher,
            debounce: Debounce::default(),
            pool,
            cache: thumbs::Cache::new(64),
            jobs,
            results,
            scaled: RefCell::new(HashMap::new()),
            undo_rect: Cell::new(None),
            paste_until: None,
            last_click: None,
            drag: None,
            pointer: None,
        })
    }

    fn resize_backing(&mut self) -> Result<(), String> {
        let pixmap = self.c.generate_id().map_err(x)?;
        self.c
            .create_pixmap(self.depth, pixmap, self.root, self.w, self.h)
            .map_err(x)?;
        let old = std::mem::replace(&mut self.back, pixmap);
        self.c.free_pixmap(old).map_err(x)?;
        Ok(())
    }

    // --- drawing primitives ------------------------------------------------

    fn fill(&self, colour: u32, rect: Rectangle) -> Result<(), String> {
        if rect.width == 0 || rect.height == 0 {
            return Ok(());
        }
        self.c
            .change_gc(self.gc, &ChangeGCAux::new().foreground(colour))
            .map_err(x)?;
        // Errors come back as events; no round trip per rectangle.
        self.c
            .poly_fill_rectangle(self.back, self.gc, &[rect])
            .map_err(x)?;
        Ok(())
    }

    fn to_x(&self, pixels: &[u8]) -> Vec<u8> {
        let mut data = Vec::with_capacity(pixels.len());
        for chunk in pixels.chunks_exact(4) {
            if self.image_lsb {
                data.extend([chunk[2], chunk[1], chunk[0], 255]);
            } else {
                data.extend([chunk[0], chunk[1], chunk[2], 255]);
            }
        }
        data
    }

    fn put(&self, x0: i16, y0: i16, w: u16, h: u16, data: &[u8]) -> Result<(), String> {
        self.c
            .put_image(
                ImageFormat::Z_PIXMAP,
                self.back,
                self.gc,
                w,
                h,
                x0,
                y0,
                0,
                self.depth,
                data,
            )
            .map_err(x)?;
        Ok(())
    }

    fn label_on(
        &self,
        x0: i16,
        y0: i16,
        text: &str,
        colour: u32,
        bg: u32,
        size: f32,
    ) -> Result<(), String> {
        let (w, h, pixels) = self.font.size(size).render(text, colour, bg);
        if w == 0 || h == 0 {
            return Ok(());
        }
        self.put(x0, y0, w as u16, h as u16, &self.to_x(&pixels))
    }

    fn label(&self, x0: i16, y0: i16, text: &str, colour: u32, size: f32) -> Result<(), String> {
        self.label_on(x0, y0, text, colour, self.palette.background, size)
    }

    fn width(&self, text: &str, size: f32) -> i16 {
        self.font.size(size).width(text).ceil() as i16
    }

    /// `text` shortened with an ellipsis to fit `max` pixels.
    fn fit(&self, text: &str, size: f32, max: i16) -> String {
        if self.width(text, size) <= max {
            return text.to_string();
        }
        let mut chars: Vec<char> = text.chars().collect();
        while !chars.is_empty() {
            chars.pop();
            let candidate: String = chars.iter().collect::<String>() + "\u{2026}";
            if self.width(&candidate, size) <= max {
                return candidate;
            }
        }
        String::new()
    }

    /// Word-wrap `text` into lines of at most `max` pixels.
    fn wrap(&self, text: &str, size: f32, max: i16) -> Vec<String> {
        let mut lines = Vec::new();
        let mut line = String::new();
        for word in text.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if self.width(&candidate, size) <= max || line.is_empty() {
                line = candidate;
            } else {
                lines.push(std::mem::take(&mut line));
                line = word.to_string();
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
        lines.into_iter().map(|l| self.fit(&l, size, max)).collect()
    }

    fn image_fill(&self, rect: Rectangle, path: &Path, stamp: &str) -> Result<(), String> {
        let Some(image) = self.cache.get(path).and_then(|slot| slot.image.as_ref()) else {
            return Ok(());
        };
        let (iw, ih) = (image.width(), image.height());
        if iw == 0 || ih == 0 || rect.width == 0 || rect.height == 0 {
            return Ok(());
        }
        let key = (path.to_path_buf(), rect.width, rect.height);
        if let Some((cached_stamp, data)) = self.scaled.borrow().get(&key) {
            if cached_stamp == stamp {
                return self.put(rect.x, rect.y, rect.width, rect.height, data);
            }
        }
        let scale =
            (f64::from(rect.width) / f64::from(iw)).max(f64::from(rect.height) / f64::from(ih));
        let tw = (f64::from(iw) * scale).round().max(f64::from(rect.width)) as u32;
        let th = (f64::from(ih) * scale).round().max(f64::from(rect.height)) as u32;
        let scaled = image::imageops::resize(image, tw, th, image::imageops::FilterType::Triangle);
        let x0 = (tw - u32::from(rect.width)) / 2;
        let y0 = (th - u32::from(rect.height)) / 2;
        let mut pixels = Vec::with_capacity(usize::from(rect.width) * usize::from(rect.height) * 4);
        for yy in y0..y0 + u32::from(rect.height) {
            for xx in x0..x0 + u32::from(rect.width) {
                pixels.extend_from_slice(&scaled.get_pixel(xx, yy).0);
            }
        }
        let data = self.to_x(&pixels);
        self.put(rect.x, rect.y, rect.width, rect.height, &data)?;
        let mut cache = self.scaled.borrow_mut();
        if cache.len() >= SCALED_CACHE {
            cache.clear();
        }
        cache.insert(key, (stamp.to_string(), data));
        Ok(())
    }

    fn outline(&self, rect: Rectangle, by: i16, colour: u32) -> Result<(), String> {
        let outer = Rectangle {
            x: rect.x - by,
            y: rect.y - by,
            width: rect.width + 2 * by as u16,
            height: rect.height + 2 * by as u16,
        };
        let right = outer.x + outer.width as i16 - 1;
        let bottom = outer.y + outer.height as i16 - 1;
        for edge in [
            Rectangle { height: 1, ..outer },
            Rectangle {
                y: bottom,
                height: 1,
                ..outer
            },
            Rectangle { width: 1, ..outer },
            Rectangle {
                x: right,
                width: 1,
                ..outer
            },
        ] {
            self.fill(colour, edge)?;
        }
        Ok(())
    }

    fn ring(&self, rect: Rectangle) -> Result<(), String> {
        self.outline(rect, 2, self.palette.foreground)
    }

    /// A button: accent when `on`, lifted on hover, dim when disabled.
    fn button(
        &self,
        rect: Rectangle,
        text: &str,
        on: bool,
        enabled: bool,
        hovered: bool,
    ) -> Result<(), String> {
        let (bg, fg) = if !enabled {
            (self.palette.background, self.palette.border)
        } else if on {
            (self.palette.accent, self.palette.background)
        } else if hovered {
            (self.palette.border, self.palette.foreground)
        } else {
            (self.palette.surface, self.palette.foreground)
        };
        self.fill(bg, rect)?;
        self.fill(
            if hovered && enabled {
                self.palette.accent
            } else {
                self.palette.border
            },
            Rectangle { height: 1, ..rect },
        )?;
        if !enabled && hovered {
            self.outline(rect, 0, self.palette.border)?;
        }
        let shown = self.fit(text, 13.0, rect.width as i16 - 14);
        let (_, text_h, _) = self.font.size(13.0).render(&shown, fg, bg);
        let y0 = rect.y + (rect.height as i16 - text_h as i16).max(0) / 2;
        self.label_on(rect.x + 8, y0, &shown, fg, bg, 13.0)
    }

    pub fn flush(&self) -> Result<(), String> {
        self.c
            .copy_area(self.back, self.win, self.gc, 0, 0, 0, 0, self.w, self.h)
            .map_err(x)?;
        self.c.flush().map_err(x)
    }

    // --- painting ----------------------------------------------------------

    pub fn paint(&self, state: &State) -> Result<(), String> {
        let l = layout(self.w, self.h);
        self.undo_rect.set(None);
        self.fill(
            self.palette.background,
            Rectangle {
                x: 0,
                y: 0,
                width: self.w,
                height: self.h,
            },
        )?;
        if l.mode == Mode::TooSmall {
            let text = "Make the window larger";
            let tw = self.width(text, 14.0);
            self.label(
                (self.w as i16 - tw) / 2,
                self.h as i16 / 2 - 10,
                text,
                self.palette.muted,
                14.0,
            )?;
            return Ok(());
        }
        self.paint_header(state, &l)?;
        self.paint_cards(state, &l)?;
        self.paint_scrollbar(state, &l)?;
        if l.mode == Mode::Compact {
            self.paint_strip(state, &l)?;
        } else {
            self.paint_panel(state, &l)?;
        }
        self.paint_actions(state, &l)?;
        self.paint_footer(state, &l)?;
        if let Some(focus) = state.focus {
            if let Some(rect) = self.focus_rect(state, &l, focus) {
                self.ring(rect)?;
            }
        }
        if let Some(modal) = &state.modal {
            self.paint_modal(state, &l, modal)?;
        }
        Ok(())
    }

    fn paint_header(&self, state: &State, l: &Layout) -> Result<(), String> {
        self.label(PAD, l.title_y, APP_NAME, self.palette.foreground, 20.0)?;
        let active = state
            .active_card()
            .map(|card| card.entry.name.clone())
            .unwrap_or_else(|| state.catalog.active.clone());
        let mut subtitle = format!("{} \u{b7} active {active}", state.position_label(l));
        if state.catalog.appearance_overrides {
            subtitle.push_str(" \u{b7} appearance overrides active");
        }
        let max = l.header_text_w as i16;
        self.label(
            PAD,
            l.subtitle_y,
            &self.fit(&subtitle, 12.0, max),
            self.palette.muted,
            12.0,
        )?;
        // The filter line.
        let f = l.filter;
        let typing = !state.filter.is_empty();
        self.fill(
            if typing {
                self.palette.surface
            } else {
                self.palette.background
            },
            f,
        )?;
        self.fill(
            if typing {
                self.palette.accent
            } else {
                self.palette.border
            },
            Rectangle {
                y: f.y + f.height as i16 - 1,
                height: 1,
                ..f
            },
        )?;
        let bg = if typing {
            self.palette.surface
        } else {
            self.palette.background
        };
        if typing {
            let text = format!("Filter: {}_", state.filter);
            let shown = self.fit(&text, 12.0, f.width as i16 - 70);
            self.label_on(f.x + 6, f.y + 5, &shown, self.palette.foreground, bg, 12.0)?;
            self.label_on(
                f.x + f.width as i16 - 62,
                f.y + 6,
                "Esc clears",
                self.palette.muted,
                bg,
                10.0,
            )?;
        } else {
            self.label_on(
                f.x + 6,
                f.y + 5,
                &self.fit("Type to filter by name or id", 12.0, f.width as i16 - 12),
                self.palette.muted,
                bg,
                12.0,
            )?;
        }
        for (chip, rect) in CHIPS.iter().zip(l.chips) {
            let text = match chip {
                layout::Chip::All => "All".to_string(),
                layout::Chip::Builtin => "Built-in".into(),
                layout::Chip::User => "User".into(),
                layout::Chip::Hidden => format!("Hidden ({})", state.hidden_count()),
            };
            let on = state.chip == *chip;
            let hovered = state.hover == Some(Hit::Chip(*chip));
            let (bg, fg) = if on {
                (self.palette.accent, self.palette.background)
            } else if hovered {
                (self.palette.border, self.palette.foreground)
            } else {
                (self.palette.surface, self.palette.muted)
            };
            self.fill(bg, rect)?;
            let shown = self.fit(&text, 11.0, rect.width as i16 - 10);
            let tw = self.width(&shown, 11.0);
            self.label_on(
                rect.x + (rect.width as i16 - tw) / 2,
                rect.y + 4,
                &shown,
                fg,
                bg,
                11.0,
            )?;
        }
        Ok(())
    }

    fn paint_cards(&self, state: &State, l: &Layout) -> Result<(), String> {
        let visible = state.visible();
        if visible.is_empty() {
            let text = if !state.filter.is_empty() {
                "No theme matches the filter. Esc clears it."
            } else if state.chip == layout::Chip::Hidden {
                "No themes are hidden."
            } else {
                "No themes here. The Hidden chip shows hidden themes."
            };
            self.label(
                l.grid.x + 20,
                l.grid.y + 30,
                &self.fit(text, 13.0, l.grid.width as i16 - 40),
                self.palette.muted,
                13.0,
            )?;
            return Ok(());
        }
        let first = state.scroll * l.cols;
        for (screen, card_index) in visible.iter().skip(first).take(l.page()).enumerate() {
            let rect = l.card(screen);
            self.paint_card(state, rect, *card_index, &state.cards[*card_index])?;
        }
        Ok(())
    }

    fn paint_card(
        &self,
        state: &State,
        rect: Rectangle,
        index: usize,
        card: &Card,
    ) -> Result<(), String> {
        let active = card.entry.id == state.catalog.active;
        let selected = state.selected.as_deref() == Some(card.entry.id.as_str());
        self.fill(card.palette.background, rect)?;
        if let Some(path) = &card.wallpaper {
            self.image_fill(
                Rectangle {
                    x: rect.x + 1,
                    y: rect.y + 1,
                    width: rect.width.saturating_sub(2),
                    height: rect.height.saturating_sub(50),
                },
                path,
                &card.stamp,
            )?;
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
        let third = rect.width as i16 / 3;
        self.fill(
            card.palette.border,
            Rectangle {
                x: rect.x + third,
                y: rect.y + 44,
                width: rect.width / 2,
                height: 54,
            },
        )?;
        self.fill(
            card.palette.surface,
            Rectangle {
                x: rect.x + third + 2,
                y: rect.y + 46,
                width: (rect.width / 2).saturating_sub(4),
                height: 50,
            },
        )?;
        self.fill(
            card.palette.accent,
            Rectangle {
                x: rect.x + third + 2,
                y: rect.y + 46,
                width: (rect.width / 2).saturating_sub(4),
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
        let name_room = rect.width as i16 - if active { 84 } else { 16 };
        self.label_on(
            footer.x + 8,
            footer.y + 4,
            &self.fit(&card.entry.name, 12.0, name_room),
            card.palette.foreground,
            card.palette.background,
            12.0,
        )?;
        let flags = card.status().to_lowercase();
        self.label_on(
            footer.x + 8,
            footer.y + 20,
            &self.fit(&flags, 10.0, rect.width as i16 - 16),
            card.palette.muted,
            card.palette.background,
            10.0,
        )?;
        if card.entry.valid {
            for (i, colour) in [
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
                        x: footer.x + 8 + i as i16 * 18,
                        y: footer.y + 34,
                        width: 14,
                        height: 10,
                    },
                )?;
            }
        } else if let Some(error) = &card.entry.error {
            self.label_on(
                footer.x + 8,
                footer.y + 34,
                &self.fit(error, 9.0, rect.width as i16 - 16),
                card.palette.accent,
                card.palette.background,
                9.0,
            )?;
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
        } else if state.hover == Some(Hit::Card(index)) {
            self.outline(rect, 1, self.palette.muted)?;
        }
        Ok(())
    }

    fn paint_scrollbar(&self, state: &State, l: &Layout) -> Result<(), String> {
        let Some(thumb) = state.scroll_thumb(l) else {
            return Ok(());
        };
        self.fill(self.palette.surface, l.track)?;
        let hot = self.drag.is_some() || state.hover == Some(Hit::Scrollbar);
        self.fill(
            if hot {
                self.palette.foreground
            } else {
                self.palette.muted
            },
            thumb,
        )
    }

    /// The family chips at the bottom of the panel: (card index, rect, label).
    fn family_chips(&self, state: &State, l: &Layout) -> Vec<(usize, Rectangle, String)> {
        if l.mode != Mode::Normal {
            return Vec::new();
        }
        let family = state.family();
        let mut out = Vec::new();
        let left = l.panel.x + 12;
        let right = l.panel.x + l.panel.width as i16 - 12;
        let (mut cx, mut row) = (left, 0i16);
        let shared = state
            .selected_card()
            .map(|c| c.entry.name.clone())
            .unwrap_or_default();
        for index in family {
            let name = &state.cards[index].entry.name;
            // Show what differs from the selected name, when they share words.
            let common: usize = name
                .split_whitespace()
                .zip(shared.split_whitespace())
                .take_while(|(a, b)| a == b)
                .count();
            let rest: Vec<&str> = name
                .split_whitespace()
                .skip(common.min(name.split_whitespace().count().saturating_sub(1)))
                .collect();
            let text = self.fit(&rest.join(" "), 10.0, right - left - 12);
            let w = self.width(&text, 10.0) + 12;
            if cx + w > right {
                row += 1;
                cx = left;
            }
            if row >= 2 {
                break;
            }
            out.push((
                index,
                Rectangle {
                    x: cx,
                    y: row * 22,
                    width: w as u16,
                    height: 18,
                },
                text,
            ));
            cx += w + 6;
        }
        let rows = out.iter().map(|(_, r, _)| r.y / 22 + 1).max().unwrap_or(0);
        let top = l.panel.y + l.panel.height as i16 - 8 - rows * 22;
        for (_, rect, _) in &mut out {
            rect.y += top;
        }
        out
    }

    fn paint_panel(&self, state: &State, l: &Layout) -> Result<(), String> {
        let p = l.panel;
        let surface = self.palette.surface;
        self.fill(surface, p)?;
        self.label_on(
            p.x + 10,
            p.y + 8,
            "Preview",
            self.palette.muted,
            surface,
            12.0,
        )?;
        let Some(card) = state.selected_card() else {
            self.label_on(
                p.x + 12,
                p.y + 40,
                "Select a theme.",
                self.palette.muted,
                surface,
                13.0,
            )?;
            return Ok(());
        };
        let palette = card.palette;
        let preview_h = (p.height as i16 - 30 - 274).clamp(80, 206);
        let preview = Rectangle {
            x: p.x + 10,
            y: p.y + 30,
            width: p.width.saturating_sub(20),
            height: preview_h as u16,
        };
        self.fill(palette.background, preview)?;
        if let Some(path) = &card.wallpaper {
            self.image_fill(preview, path, &card.stamp)?;
        }
        let bar = Rectangle {
            height: 24,
            ..preview
        };
        self.fill(palette.surface, bar)?;
        for i in 0..8 {
            self.fill(
                if i == 0 {
                    palette.accent
                } else {
                    palette.border
                },
                Rectangle {
                    x: bar.x + 8 + i * 14,
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
        if preview_h >= 150 {
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
            let launcher = Rectangle {
                x: window.x + 8,
                y: window.y + 50,
                width: window.width.saturating_sub(16),
                height: 22,
            };
            self.fill(palette.background, launcher)?;
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
                    ..launcher
                },
            )?;
        }
        // Swatches with their hex values.
        let hex: Vec<String> = match &card.entry.colors {
            Some(c) => [&c.background, &c.foreground, &c.muted, &c.border, &c.focus]
                .iter()
                .map(|v| v.trim_start_matches('#').to_lowercase())
                .collect(),
            None => [
                palette.background,
                palette.foreground,
                palette.muted,
                palette.border,
                palette.accent,
            ]
            .iter()
            .map(|v| format!("{v:06x}"))
            .collect(),
        };
        let mut y = preview.y + preview.height as i16 + 12;
        for (i, (name, colour)) in ["bg", "fg", "muted", "border", "focus"]
            .into_iter()
            .zip([
                palette.background,
                palette.foreground,
                palette.muted,
                palette.border,
                palette.accent,
            ])
            .enumerate()
        {
            let sx = p.x + 12 + (i as i16 % 2) * 140;
            let sy = y + (i as i16 / 2) * 16;
            self.fill(
                self.palette.border,
                Rectangle {
                    x: sx - 1,
                    y: sy - 1,
                    width: 16,
                    height: 14,
                },
            )?;
            self.fill(
                colour,
                Rectangle {
                    x: sx,
                    y: sy,
                    width: 14,
                    height: 12,
                },
            )?;
            self.label_on(
                sx + 20,
                sy,
                &format!("{name} {}", hex.get(i).map(String::as_str).unwrap_or("")),
                self.palette.muted,
                surface,
                10.0,
            )?;
        }
        y += 3 * 16 + 8;
        let family = self.family_chips(state, l);
        let limit = family
            .first()
            .map(|(_, r, _)| r.y - 20)
            .unwrap_or(p.y + p.height as i16 - 6);
        let room = p.width as i16 - 24;
        let active = card.entry.id == state.catalog.active;
        let mut lines: Vec<(String, u32, f32, i16)> = vec![
            (card.entry.name.clone(), self.palette.foreground, 14.0, 22),
            (
                format!("{} \u{b7} {}", card.entry.id, card.status()),
                self.palette.muted,
                10.0,
                18,
            ),
            (
                if active {
                    "Active theme"
                } else {
                    "Not applied"
                }
                .into(),
                if active {
                    palette.accent
                } else {
                    self.palette.muted
                },
                11.0,
                18,
            ),
        ];
        let file = card
            .wallpaper
            .as_deref()
            .and_then(|path| path.file_name())
            .map(|n| n.to_string_lossy().to_string());
        let wallpaper = match (&file, card.entry.wallpaper_available) {
            (Some(file), true) => {
                match card
                    .wallpaper
                    .as_ref()
                    .and_then(|p| self.cache.get(p))
                    .and_then(|slot| slot.dims)
                {
                    Some((w, h)) => format!("{file} \u{b7} {w}\u{d7}{h}"),
                    None => file.clone(),
                }
            }
            (Some(file), false) => format!("{file} is missing \u{b7} colour fallback"),
            (None, _) => "No wallpaper \u{b7} theme colour fallback".into(),
        };
        lines.push((wallpaper, self.palette.muted, 10.0, 18));
        if !card.entry.valid {
            let error = card.entry.error.as_deref().unwrap_or("invalid theme");
            for line in self.wrap(error, 10.0, room) {
                lines.push((line, palette.accent, 10.0, 14));
            }
        }
        if state.catalog.appearance_overrides && active {
            lines.push((
                "appearance.toml colour overrides apply".into(),
                self.palette.muted,
                10.0,
                18,
            ));
        }
        for (text, colour, size, step) in lines {
            if y + step > limit {
                break;
            }
            self.label_on(
                p.x + 12,
                y,
                &self.fit(&text, size, room),
                colour,
                surface,
                size,
            )?;
            y += step;
        }
        if let Some((_, first, _)) = family.first() {
            self.label_on(
                p.x + 12,
                first.y - 17,
                "Same family",
                self.palette.muted,
                surface,
                10.0,
            )?;
        }
        for (index, rect, text) in family {
            let hovered = state.hover == Some(Hit::Family(index));
            let bg = if hovered {
                self.palette.border
            } else {
                self.palette.background
            };
            self.fill(bg, rect)?;
            self.label_on(
                rect.x + 6,
                rect.y + 3,
                &text,
                self.palette.foreground,
                bg,
                10.0,
            )?;
        }
        Ok(())
    }

    /// The compact layout's collapsed preview.
    fn paint_strip(&self, state: &State, l: &Layout) -> Result<(), String> {
        let p = l.panel;
        let surface = self.palette.surface;
        self.fill(surface, p)?;
        let Some(card) = state.selected_card() else {
            return self.label_on(
                p.x + 10,
                p.y + 18,
                "Select a theme.",
                self.palette.muted,
                surface,
                12.0,
            );
        };
        let palette = card.palette;
        for (i, colour) in [
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
                    x: p.x + 10 + i as i16 * 18,
                    y: p.y + 10,
                    width: 14,
                    height: 32,
                },
            )?;
        }
        let tx = p.x + 110;
        let room = p.width as i16 - 120;
        self.label_on(
            tx,
            p.y + 6,
            &self.fit(&card.entry.name, 13.0, room),
            self.palette.foreground,
            surface,
            13.0,
        )?;
        let active = card.entry.id == state.catalog.active;
        let second = if !card.entry.valid {
            card.entry
                .error
                .clone()
                .unwrap_or_else(|| "invalid theme".into())
        } else {
            format!(
                "{} \u{b7} {}",
                card.status(),
                if active { "active" } else { "not applied" }
            )
        };
        self.label_on(
            tx,
            p.y + 28,
            &self.fit(&second, 10.0, room),
            if card.entry.valid {
                self.palette.muted
            } else {
                palette.accent
            },
            surface,
            10.0,
        )
    }

    fn paint_actions(&self, state: &State, l: &Layout) -> Result<(), String> {
        let card = state.selected_card();
        let active = card.is_some_and(|c| c.entry.id == state.catalog.active);
        let hidden = card.is_some_and(|c| c.entry.hidden);
        let compact = l.mode == Mode::Compact;
        for (button, rect) in BUTTONS.iter().zip(l.buttons) {
            let text = match button {
                Button::Apply => "Apply",
                Button::Import => {
                    if compact {
                        "Import\u{2026}"
                    } else {
                        "Import theme\u{2026}"
                    }
                }
                Button::Remove => "Remove",
                Button::Hide => match (hidden, compact) {
                    (true, false) => "Show theme",
                    (false, false) => "Hide theme",
                    (true, true) => "Show",
                    (false, true) => "Hide",
                },
                Button::Reload => "Reload",
            };
            let enabled = state.disabled_reason(*button).is_none();
            let hovered = state.hover == Some(Hit::Button(*button));
            self.button(
                rect,
                text,
                *button == Button::Apply && active,
                enabled,
                hovered,
            )?;
        }
        if let (Some(y0), Some(reason)) = (l.reason_y, self.reason(state)) {
            self.label(
                PAD,
                y0,
                &self.fit(&reason, 11.0, l.w as i16 - PAD * 2),
                self.palette.muted,
                11.0,
            )?;
        }
        Ok(())
    }

    /// Why the hovered or focused button is disabled.
    fn reason(&self, state: &State) -> Option<String> {
        [state.hover, state.focus]
            .into_iter()
            .flatten()
            .find_map(|hit| match hit {
                Hit::Button(button) => state.disabled_reason(button),
                _ => None,
            })
    }

    fn paint_footer(&self, state: &State, l: &Layout) -> Result<(), String> {
        let compact_reason = if l.reason_y.is_none() {
            self.reason(state)
        } else {
            None
        };
        let (text, colour) = if let Some(reason) = compact_reason {
            (reason, self.palette.muted)
        } else if state.busy.is_some() {
            (state.message.clone(), self.palette.accent)
        } else if state.message_is_error {
            (
                state.message.clone(),
                state
                    .selected_card()
                    .map(|card| card.palette.accent)
                    .unwrap_or(self.palette.accent),
            )
        } else {
            (state.message.clone(), self.palette.muted)
        };
        let room = l.message.width as i16;
        let undo = state.undo.as_ref().filter(|_| state.busy.is_none());
        let shown = self.fit(&text, 12.0, room - if undo.is_some() { 70 } else { 0 });
        self.label(l.message.x, l.message.y, &shown, colour, 12.0)?;
        if undo.is_some() {
            let x0 = l.message.x + self.width(&shown, 12.0) + 10;
            let rect = Rectangle {
                x: x0,
                y: l.message.y - 2,
                width: 56,
                height: 20,
            };
            let hovered = state.hover == Some(Hit::Undo);
            let bg = if hovered {
                self.palette.accent
            } else {
                self.palette.surface
            };
            let fg = if hovered {
                self.palette.background
            } else {
                self.palette.foreground
            };
            self.fill(bg, rect)?;
            self.label_on(rect.x + 10, rect.y + 3, "Undo", fg, bg, 12.0)?;
            self.undo_rect.set(Some(rect));
        }
        let hints = if l.mode == Mode::Compact {
            "Type to filter  Enter apply  Alt+I import  Ctrl+Z undo  F1 help  Esc close"
        } else {
            "Type to filter  Tab focus  arrows choose  Enter apply  Alt+I import  Alt+H hide  Alt+D remove  Alt+V hidden  Ctrl+Z undo  F1 help  Esc close"
        };
        self.label(
            PAD,
            l.hints_y,
            &self.fit(hints, 11.0, l.w as i16 - PAD * 2),
            self.palette.muted,
            11.0,
        )
    }

    fn focus_rect(&self, state: &State, l: &Layout, focus: Hit) -> Option<Rectangle> {
        match focus {
            Hit::Card(index) => {
                let visible = state.visible();
                let position = visible.iter().position(|i| *i == index)?;
                let first = state.scroll * l.cols;
                if position < first || position >= first + l.page() {
                    return None;
                }
                Some(l.card(position - first))
            }
            Hit::Button(button) => Some(l.button(button)),
            Hit::Chip(chip) => Some(l.chip(chip)),
            _ => None,
        }
    }

    // --- dialogs -----------------------------------------------------------

    fn paint_modal(&self, state: &State, l: &Layout, modal: &Modal) -> Result<(), String> {
        let m = l.modal();
        self.fill(self.palette.background, m)?;
        self.outline(m, 0, self.palette.border)?;
        self.fill(self.palette.accent, Rectangle { height: 2, ..m })?;
        match modal {
            Modal::Import(form) => self.paint_import(state, m, form),
            Modal::Browse(browse) => self.paint_browse(state, m, browse),
            Modal::ConfirmRemove { id, name } => {
                let room = m.width as i16 - 48;
                self.label(
                    m.x + 24,
                    m.y + 30,
                    "Remove theme?",
                    self.palette.foreground,
                    18.0,
                )?;
                self.label(
                    m.x + 24,
                    m.y + 68,
                    &self.fit(&format!("Remove {name} ({id})?"), 13.0, room),
                    self.palette.muted,
                    13.0,
                )?;
                self.label(
                    m.x + 24,
                    m.y + 96,
                    &self.fit(
                        "Only this user theme and a recorded imported wallpaper are removed.",
                        11.0,
                        room,
                    ),
                    self.palette.muted,
                    11.0,
                )?;
                self.label(
                    m.x + 24,
                    m.y + 132,
                    "Enter confirms. Escape keeps the theme.",
                    self.palette.muted,
                    11.0,
                )?;
                let (confirm, cancel) = confirm_rects(m);
                self.button(
                    confirm,
                    "Remove theme",
                    true,
                    true,
                    state.hover == Some(Hit::Confirm),
                )?;
                self.button(
                    cancel,
                    "Keep it",
                    false,
                    true,
                    state.hover == Some(Hit::Cancel),
                )
            }
            Modal::Help => {
                self.label(m.x + 24, m.y + 18, APP_NAME, self.palette.foreground, 18.0)?;
                for (i, line) in help_lines().into_iter().enumerate() {
                    let y0 = m.y + 52 + i as i16 * 19;
                    if y0 + 16 > m.y + m.height as i16 {
                        break;
                    }
                    self.label(
                        m.x + 24,
                        y0,
                        &self.fit(line, 11.0, m.width as i16 - 48),
                        self.palette.muted,
                        11.0,
                    )?;
                }
                Ok(())
            }
        }
    }

    fn status_label(&self, right: i16, y0: i16, status: Option<&PathStatus>) -> Result<(), String> {
        let Some(status) = status else {
            return Ok(());
        };
        let text = status.label();
        if text.is_empty() {
            return Ok(());
        }
        let text = self.fit(&text, 11.0, 360);
        let colour = if status.ok() {
            self.palette.foreground
        } else {
            self.palette.accent
        };
        self.label(right - self.width(&text, 11.0), y0, &text, colour, 11.0)
    }

    fn paint_field(
        &self,
        rect: Rectangle,
        field: &crate::state::Field,
        focused: bool,
    ) -> Result<(), String> {
        let bg = if focused {
            self.palette.surface
        } else {
            self.palette.background
        };
        self.fill(bg, rect)?;
        self.fill(
            if focused {
                self.palette.accent
            } else {
                self.palette.border
            },
            Rectangle {
                y: rect.y + rect.height as i16 - 1,
                height: 1,
                ..rect
            },
        )?;
        let room = rect.width as i16 - 14;
        let chars: Vec<char> = field.text.chars().collect();
        let cursor = field.cursor.min(chars.len());
        // Scroll the text so the cursor stays in view.
        let mut start = 0;
        while start < cursor
            && self.width(&chars[start..cursor].iter().collect::<String>(), 11.0) > room - 4
        {
            start += 1;
        }
        let tail: String = chars[start..].iter().collect();
        let shown = self.fit(&tail, 11.0, room);
        if chars.is_empty() && !focused {
            self.label_on(
                rect.x + 6,
                rect.y + 7,
                "\u{2026}",
                self.palette.muted,
                bg,
                11.0,
            )?;
        } else {
            self.label_on(
                rect.x + 6,
                rect.y + 7,
                &shown,
                self.palette.foreground,
                bg,
                11.0,
            )?;
        }
        if focused {
            let before: String = chars[start..cursor].iter().collect();
            let cx = rect.x + 6 + self.width(&before, 11.0);
            self.fill(
                self.palette.accent,
                Rectangle {
                    x: cx,
                    y: rect.y + 5,
                    width: 1,
                    height: rect.height.saturating_sub(10),
                },
            )?;
        }
        Ok(())
    }

    fn paint_import(&self, state: &State, m: Rectangle, form: &ImportForm) -> Result<(), String> {
        let r = import_rects(m);
        let right = m.x + m.width as i16 - 24;
        let busy = state.busy.is_some();
        self.label(
            m.x + 24,
            m.y + 16,
            "Import theme",
            self.palette.foreground,
            16.0,
        )?;
        self.label(
            m.x + 24,
            r.fields[0].y - 18,
            "Theme TOML file",
            self.palette.muted,
            11.0,
        )?;
        self.status_label(right, r.fields[0].y - 18, form.theme_status.as_ref())?;
        self.label(
            m.x + 24,
            r.fields[1].y - 18,
            "Optional wallpaper (PNG only)",
            self.palette.muted,
            11.0,
        )?;
        self.status_label(right, r.fields[1].y - 18, form.wallpaper_status.as_ref())?;
        self.label(
            m.x + 24,
            r.fields[2].y - 18,
            "Optional theme id (default: the file name)",
            self.palette.muted,
            11.0,
        )?;
        for (i, rect) in r.fields.iter().enumerate() {
            self.paint_field(*rect, &form.fields[i], form.focus == i && !busy)?;
        }
        for (i, rect) in r.browse.iter().enumerate() {
            self.button(
                *rect,
                "Browse\u{2026}",
                false,
                !busy,
                state.hover == Some(Hit::BrowseFor(i)),
            )?;
        }
        // What the theme file says, before anything is imported.
        let py = r.fields[0].y + r.fields[0].height as i16 + 6;
        match &form.preview {
            Some(Ok(preview)) => {
                self.label(
                    m.x + 24,
                    py,
                    &self.fit(&format!("Theme: {}", preview.name), 11.0, 260),
                    self.palette.foreground,
                    11.0,
                )?;
                for (i, colour) in preview.colors.iter().enumerate() {
                    let sx = m.x + 290 + i as i16 * 72;
                    if sx + 70 > right {
                        break;
                    }
                    self.fill(
                        *colour,
                        Rectangle {
                            x: sx,
                            y: py + 1,
                            width: 12,
                            height: 12,
                        },
                    )?;
                    self.label(
                        sx + 16,
                        py,
                        &format!("{colour:06x}"),
                        self.palette.muted,
                        10.0,
                    )?;
                }
            }
            Some(Err(error)) => {
                self.label(
                    m.x + 24,
                    py,
                    &self.fit(error, 11.0, m.width as i16 - 48),
                    self.palette.accent,
                    11.0,
                )?;
            }
            None => {}
        }
        let box_bg = if form.replace {
            self.palette.accent
        } else {
            self.palette.surface
        };
        let hovered = state.hover == Some(Hit::Replace);
        self.fill(
            box_bg,
            Rectangle {
                x: r.replace.x,
                y: r.replace.y + 4,
                width: 14,
                height: 14,
            },
        )?;
        if hovered {
            self.outline(
                Rectangle {
                    x: r.replace.x,
                    y: r.replace.y + 4,
                    width: 14,
                    height: 14,
                },
                1,
                self.palette.muted,
            )?;
        }
        self.label(
            r.replace.x + 22,
            r.replace.y + 4,
            &self.fit(
                "Replace a user theme with the same id (Ctrl+R)",
                11.0,
                r.replace.width as i16 - 24,
            ),
            self.palette.muted,
            11.0,
        )?;
        if let Some(error) = &form.error {
            for (i, line) in self
                .wrap(error, 11.0, m.width as i16 - 48)
                .into_iter()
                .take(2)
                .enumerate()
            {
                self.label(
                    m.x + 24,
                    r.replace.y + 28 + i as i16 * 15,
                    &line,
                    self.palette.accent,
                    11.0,
                )?;
            }
        }
        self.button(
            r.submit,
            if busy { "Importing\u{2026}" } else { "Import" },
            !busy,
            !busy,
            state.hover == Some(Hit::Submit),
        )?;
        self.button(
            r.cancel,
            "Cancel",
            false,
            true,
            state.hover == Some(Hit::Cancel),
        )?;
        let hx = r.cancel.x + r.cancel.width as i16 + 16;
        self.label(
            hx,
            r.submit.y + 7,
            &self.fit(
                "Tab next  Ctrl+V paste  Ctrl+B browse  Enter import  Esc cancel",
                10.0,
                right - hx,
            ),
            self.palette.muted,
            10.0,
        )
    }

    fn paint_browse(&self, state: &State, m: Rectangle, browse: &Browse) -> Result<(), String> {
        let r = browse_rects(m);
        let room = m.width as i16 - 48;
        let title = match browse.want {
            Want::Theme => "Choose a theme TOML file",
            Want::Wallpaper => "Choose a PNG wallpaper",
        };
        self.label(m.x + 24, m.y + 16, title, self.palette.foreground, 16.0)?;
        let shown = import::display_path(&browse.dir, import::home().as_deref());
        self.label(
            m.x + 24,
            m.y + 44,
            &self.fit(&shown, 11.0, room),
            self.palette.muted,
            11.0,
        )?;
        self.button(
            r.downloads,
            "Downloads",
            false,
            true,
            state.hover == Some(Hit::BrowseDownloads),
        )?;
        self.button(
            r.home,
            "Home",
            false,
            true,
            state.hover == Some(Hit::BrowseHome),
        )?;
        self.button(r.up, "Up", false, true, state.hover == Some(Hit::BrowseUp))?;
        self.button(
            r.back,
            "Back",
            false,
            true,
            state.hover == Some(Hit::Cancel),
        )?;
        if let Some(error) = &browse.error {
            self.label(
                m.x + 24,
                r.list.y + 4,
                &self.fit(error, 11.0, room),
                self.palette.accent,
                11.0,
            )?;
        } else if browse.entries.is_empty() {
            let text = match browse.want {
                Want::Theme => "No folders or .toml files here.",
                Want::Wallpaper => "No folders or .png files here.",
            };
            self.label(m.x + 24, r.list.y + 4, text, self.palette.muted, 11.0)?;
        }
        for (row, (index, entry)) in browse
            .entries
            .iter()
            .enumerate()
            .skip(browse.scroll)
            .take(r.rows)
            .enumerate()
        {
            let rect = Rectangle {
                x: r.list.x,
                y: r.list.y + row as i16 * BROWSE_ROW,
                width: r.list.width,
                height: BROWSE_ROW as u16 - 2,
            };
            let selected = index == browse.selected;
            let hovered = state.hover == Some(Hit::BrowseRow(index));
            let bg = if selected {
                self.palette.surface
            } else if hovered {
                self.palette.border
            } else {
                self.palette.background
            };
            self.fill(bg, rect)?;
            if selected {
                self.fill(self.palette.accent, Rectangle { width: 3, ..rect })?;
            }
            let text = if entry.dir {
                format!("\u{25b8} {}/", entry.name)
            } else {
                format!("  {}", entry.name)
            };
            self.label_on(
                rect.x + 10,
                rect.y + 4,
                &self.fit(&text, 11.0, rect.width as i16 - 20),
                if entry.dir {
                    self.palette.foreground
                } else {
                    self.palette.muted
                },
                bg,
                11.0,
            )?;
        }
        self.label(
            m.x + 24,
            r.back.y + 7,
            &self.fit(
                "Enter open/choose  Backspace up  Alt+H home  Alt+D Downloads  Esc back",
                10.0,
                r.back.x - m.x - 40,
            ),
            self.palette.muted,
            10.0,
        )
    }

    // --- hit testing -------------------------------------------------------

    fn hit(&self, state: &State, l: &Layout, px: i16, py: i16) -> Option<Hit> {
        if l.mode == Mode::TooSmall {
            return None;
        }
        if let Some(modal) = &state.modal {
            let m = l.modal();
            return match modal {
                Modal::Import(_) => {
                    let r = import_rects(m);
                    for (i, rect) in r.fields.iter().enumerate() {
                        if contains(*rect, px, py) {
                            return Some(Hit::Field(i));
                        }
                    }
                    for (i, rect) in r.browse.iter().enumerate() {
                        if contains(*rect, px, py) {
                            return Some(Hit::BrowseFor(i));
                        }
                    }
                    [
                        (r.replace, Hit::Replace),
                        (r.submit, Hit::Submit),
                        (r.cancel, Hit::Cancel),
                    ]
                    .into_iter()
                    .find(|(rect, _)| contains(*rect, px, py))
                    .map(|(_, hit)| hit)
                }
                Modal::Browse(browse) => {
                    let r = browse_rects(m);
                    if contains(r.list, px, py) {
                        let row = ((py - r.list.y) / BROWSE_ROW) as usize;
                        let index = browse.scroll + row;
                        return (row < r.rows && index < browse.entries.len())
                            .then_some(Hit::BrowseRow(index));
                    }
                    [
                        (r.downloads, Hit::BrowseDownloads),
                        (r.home, Hit::BrowseHome),
                        (r.up, Hit::BrowseUp),
                        (r.back, Hit::Cancel),
                    ]
                    .into_iter()
                    .find(|(rect, _)| contains(*rect, px, py))
                    .map(|(_, hit)| hit)
                }
                Modal::ConfirmRemove { .. } => {
                    let (confirm, cancel) = confirm_rects(m);
                    if contains(confirm, px, py) {
                        Some(Hit::Confirm)
                    } else if contains(cancel, px, py) {
                        Some(Hit::Cancel)
                    } else {
                        None
                    }
                }
                Modal::Help => Some(Hit::Cancel),
            };
        }
        if let Some(rect) = self.undo_rect.get() {
            if contains(rect, px, py) {
                return Some(Hit::Undo);
            }
        }
        if contains(l.track, px, py) && state.scroll_thumb(l).is_some() {
            return Some(Hit::Scrollbar);
        }
        let visible = state.visible();
        let first = state.scroll * l.cols;
        for (screen, index) in visible.iter().skip(first).take(l.page()).enumerate() {
            if contains(l.card(screen), px, py) {
                return Some(Hit::Card(*index));
            }
        }
        for (button, rect) in BUTTONS.iter().zip(l.buttons) {
            if contains(rect, px, py) {
                return Some(Hit::Button(*button));
            }
        }
        for (chip, rect) in CHIPS.iter().zip(l.chips) {
            if contains(rect, px, py) {
                return Some(Hit::Chip(*chip));
            }
        }
        self.family_chips(state, l)
            .into_iter()
            .find(|(_, rect, _)| contains(*rect, px, py))
            .map(|(index, _, _)| Hit::Family(index))
    }

    // --- previews ----------------------------------------------------------

    /// The wallpapers on screen now (one row of look-ahead) and the
    /// selected one.
    fn wanted(&self, state: &State, l: &Layout) -> Vec<(PathBuf, String)> {
        let visible = state.visible();
        let first = state.scroll * l.cols;
        let mut out: Vec<(PathBuf, String)> = Vec::new();
        let mut push = |card: &Card| {
            if let Some(path) = &card.wallpaper {
                if !out.iter().any(|(p, _)| p == path) {
                    out.push((path.clone(), card.stamp.clone()));
                }
            }
        };
        if let Some(card) = state.selected_card() {
            push(card);
        }
        for index in visible.iter().skip(first).take(l.page() + l.cols) {
            push(&state.cards[*index]);
        }
        out
    }

    fn request_previews(&mut self, state: &State) {
        let l = layout(self.w, self.h);
        let wanted = self.wanted(state, &l);
        self.cache.touch(wanted.iter().map(|(p, _)| p));
        for (path, stamp) in wanted {
            if self.cache.wants(&path, &stamp) && self.pool.request(path.clone(), stamp) {
                self.cache.requested(path);
            }
        }
    }

    fn collect_previews(&mut self, state: &State) -> bool {
        let l = layout(self.w, self.h);
        let protected: HashSet<PathBuf> =
            self.wanted(state, &l).into_iter().map(|(p, _)| p).collect();
        let mut changed = false;
        while let Some(done) = self.pool.try_recv() {
            self.cache.arrived(&done.path);
            let current = state.cards.iter().any(|card| {
                card.wallpaper.as_ref() == Some(&done.path) && card.stamp == done.stamp
            });
            if current {
                self.cache
                    .insert(done.path, done.stamp, done.preview, &protected);
                changed = true;
            }
        }
        changed
    }

    fn watch(&mut self, state: &State) {
        self.watcher
            .set(watch::watched_dirs(&state.catalog.theme_dir));
    }

    // --- clipboard ---------------------------------------------------------

    fn request_paste(&mut self) -> Result<(), String> {
        self.c
            .delete_property(self.win, self.atoms.paste)
            .map_err(x)?;
        self.c
            .convert_selection(
                self.win,
                self.atoms.clipboard,
                self.atoms.utf8,
                self.atoms.paste,
                x11rb::CURRENT_TIME,
            )
            .map_err(x)?;
        self.paste_until = Some(Instant::now() + PASTE_DEADLINE);
        Ok(())
    }

    fn take_paste(
        &mut self,
        event: &SelectionNotifyEvent,
        state: &mut State,
    ) -> Result<(), String> {
        if self.paste_until.take().is_none() || event.requestor != self.win {
            return Ok(());
        }
        if event.property == x11rb::NONE {
            state.set_message("The clipboard holds no text to paste.", true);
            return Ok(());
        }
        let reply = self
            .c
            .get_property(true, self.win, self.atoms.paste, AtomEnum::ANY, 0, 1 << 16)
            .map_err(x)?
            .reply()
            .map_err(x)?;
        if reply.type_ == self.atoms.incr {
            state.set_message("The clipboard text is too large to paste.", true);
            return Ok(());
        }
        state.paste(&String::from_utf8_lossy(&reply.value));
        Ok(())
    }
}

/// The action worker: one thread, one action at a time, each bounded by its
/// niructl deadline, then a catalogue read. It wakes the loop when done.
fn spawn_worker(wake: Arc<Wake>) -> (Sender<(u64, Action)>, Receiver<WorkerResult>) {
    let (jobs, work) = mpsc::channel::<(u64, Action)>();
    let (done, results) = mpsc::channel::<WorkerResult>();
    std::thread::spawn(move || {
        while let Ok((serial, action)) = work.recv() {
            let (result, catalog) = action.perform();
            if done.send((serial, result, catalog)).is_err() {
                return;
            }
            wake.notify();
        }
    });
    (jobs, results)
}

struct ImportRects {
    fields: [Rectangle; 3],
    browse: [Rectangle; 2],
    replace: Rectangle,
    submit: Rectangle,
    cancel: Rectangle,
}

fn import_rects(m: Rectangle) -> ImportRects {
    let inner = m.width as i16 - 48;
    let path_w = (inner - 110).max(40) as u16;
    let field = |y: i16, width: u16| Rectangle {
        x: m.x + 24,
        y: m.y + y,
        width,
        height: 28,
    };
    let fields = [
        field(70, path_w),
        field(150, path_w),
        field(214, inner.max(40) as u16),
    ];
    let browse = [0, 1].map(|i| Rectangle {
        x: fields[i].x + path_w as i16 + 10,
        y: fields[i].y,
        width: 100,
        height: 28,
    });
    let bottom = m.y + m.height as i16;
    ImportRects {
        fields,
        browse,
        replace: Rectangle {
            x: m.x + 24,
            y: m.y + 252,
            width: inner.clamp(40, 420) as u16,
            height: 22,
        },
        submit: Rectangle {
            x: m.x + 24,
            y: bottom - 46,
            width: 130,
            height: 28,
        },
        cancel: Rectangle {
            x: m.x + 164,
            y: bottom - 46,
            width: 100,
            height: 28,
        },
    }
}

struct BrowseRects {
    downloads: Rectangle,
    home: Rectangle,
    up: Rectangle,
    back: Rectangle,
    list: Rectangle,
    rows: usize,
}

fn browse_rects(m: Rectangle) -> BrowseRects {
    let button = |x: i16, width: u16| Rectangle {
        x: m.x + x,
        y: m.y + 64,
        width,
        height: 26,
    };
    let bottom = m.y + m.height as i16;
    let list = Rectangle {
        x: m.x + 24,
        y: m.y + 100,
        width: m.width.saturating_sub(48),
        height: (bottom - 52 - (m.y + 100)).max(BROWSE_ROW) as u16,
    };
    BrowseRects {
        downloads: button(24, 110),
        home: button(140, 80),
        up: button(226, 70),
        back: Rectangle {
            x: m.x + m.width as i16 - 124,
            y: bottom - 42,
            width: 100,
            height: 28,
        },
        rows: (list.height as i16 / BROWSE_ROW).max(1) as usize,
        list,
    }
}

fn confirm_rects(m: Rectangle) -> (Rectangle, Rectangle) {
    (
        Rectangle {
            x: m.x + 24,
            y: m.y + 170,
            width: 170,
            height: 30,
        },
        Rectangle {
            x: m.x + 204,
            y: m.y + 170,
            width: 120,
            height: 30,
        },
    )
}

pub fn help_lines() -> Vec<&'static str> {
    vec![
        "Type                filter the grid by name or id; Esc clears, Esc again closes",
        "Tab / Shift+Tab     move between cards, actions and filter chips",
        "Arrows              choose a theme card (only the preview changes)",
        "PageUp/PageDown     move a page; Home/End to the first or last theme",
        "Enter               apply the selected theme, or press the focused control",
        "Double-click        apply a card",
        "Ctrl+Z or Undo      re-apply the theme that was active before (15 s)",
        "Alt+A               apply the selected theme",
        "Alt+I               import a theme TOML and an optional PNG wallpaper",
        "Alt+H               hide/show the selected theme",
        "Alt+D               remove a user theme",
        "Alt+V               show the hidden themes (the Hidden chip)",
        "Alt+R               reload the catalogue",
        "Ctrl+Q / Alt+Q      close the window",
        "Import fields       Ctrl+V or Shift+Insert paste, Ctrl+B browse, Ctrl+R replace",
        "The preview is representative; Apply uses the existing NIRUWM theme path.",
    ]
}

fn refresh_now(app: &mut App, state: &mut State) {
    let l = layout(app.w, app.h);
    match backend::catalog() {
        Ok(catalog) => {
            state.apply_catalog(catalog, theme::load(), &l);
        }
        Err(error) => state.set_message(error, true),
    }
    app.watch(state);
}

fn run_command(app: &mut App, state: &mut State, command: Command) -> Result<bool, String> {
    match command {
        Command::None | Command::Redraw => {}
        Command::Quit => return Ok(false),
        Command::Start(action) => {
            let serial = state.busy.as_ref().map(|b| b.serial).unwrap_or(0);
            if app.jobs.send((serial, action)).is_err() {
                state.abandon();
            }
        }
        Command::Paste => app.request_paste()?,
        Command::Reload => {
            refresh_now(app, state);
            if !state.message_is_error {
                state.set_message("Reloaded the theme catalogue.", false);
            }
        }
    }
    Ok(true)
}

/// Handle one X event. Returns false when the window should close.
fn handle_event(app: &mut App, state: &mut State, event: Event) -> Result<(bool, bool), String> {
    let mut dirty = true;
    let now = Instant::now();
    match event {
        Event::Expose(e) => dirty = e.count == 0,
        Event::NoExposure(_) => dirty = false,
        Event::ConfigureNotify(e) => {
            if e.width != app.w || e.height != app.h {
                // Paint for the real size: a tiling window manager may give
                // less than the size hints ask for.
                app.w = e.width.max(1);
                app.h = e.height.max(1);
                app.resize_backing()?;
                let l = layout(app.w, app.h);
                state.follow_selection(&l);
            } else {
                dirty = false;
            }
        }
        Event::MappingNotify(e) if e.request != Mapping::POINTER => {
            app.keys = Keys::load(&app.c)?;
            dirty = false;
        }
        Event::FocusIn(_) => {
            app.debounce.note(now);
            dirty = false;
        }
        Event::KeyPress(e) => {
            let l = layout(app.w, app.h);
            let key = Key {
                sym: app.keys.sym(e.detail, e.state),
                base: app.keys.base(e.detail),
                shift: e.state.contains(KeyButMask::SHIFT),
                ctrl: e.state.contains(KeyButMask::CONTROL),
                alt: e.state.contains(KeyButMask::MOD1),
            };
            let command = state.handle_key(key, &l, now);
            if command == Command::None {
                dirty = false;
            }
            if !run_command(app, state, command)? {
                return Ok((false, false));
            }
        }
        Event::MotionNotify(e) => {
            app.pointer = Some((e.event_x, e.event_y));
            let l = layout(app.w, app.h);
            if let Some(grab) = app.drag {
                let scroll = state.scroll_for_thumb(&l, e.event_y - grab);
                dirty = scroll != state.scroll;
                state.scroll = scroll;
            } else {
                let hover = app.hit(state, &l, e.event_x, e.event_y);
                dirty = hover != state.hover;
                state.hover = hover;
            }
        }
        Event::LeaveNotify(_) => {
            app.pointer = None;
            dirty = state.hover.is_some();
            state.hover = None;
        }
        Event::ButtonRelease(e) => {
            dirty = app.drag.take().is_some() && e.detail == 1;
        }
        Event::ButtonPress(e) => {
            let l = layout(app.w, app.h);
            let shift = e.state.contains(KeyButMask::SHIFT);
            match e.detail {
                4 | 5 => {
                    let down = e.detail == 5;
                    if let Some(Modal::Browse(browse)) = state.modal.as_mut() {
                        let rows = browse_rects(l.modal()).rows;
                        let max = browse.entries.len().saturating_sub(rows);
                        browse.scroll = if down {
                            (browse.scroll + 3).min(max)
                        } else {
                            browse.scroll.saturating_sub(3)
                        };
                    } else if state.modal.is_none() {
                        // The wheel moves the view by rows (Shift: a page);
                        // it never changes the selection.
                        let step = if shift { l.rows as i64 } else { 1 };
                        state.scroll_by(&l, if down { step } else { -step });
                    }
                    let hover = app.hit(state, &l, e.event_x, e.event_y);
                    state.hover = hover;
                }
                1 => {
                    let hit = app.hit(state, &l, e.event_x, e.event_y);
                    let double = matches!((hit, app.last_click), (Some(h), Some((last, t))) if h == last && e.time.wrapping_sub(t) < DOUBLE_CLICK_MS);
                    app.last_click = if double {
                        None
                    } else {
                        hit.map(|h| (h, e.time))
                    };
                    let command = match hit {
                        None => Command::None,
                        Some(Hit::Scrollbar) => {
                            if let Some(thumb) = state.scroll_thumb(&l) {
                                if contains(thumb, e.event_x, e.event_y) {
                                    app.drag = Some(e.event_y - thumb.y);
                                } else {
                                    let page = l.rows as i64;
                                    state.scroll_by(
                                        &l,
                                        if e.event_y < thumb.y { -page } else { page },
                                    );
                                }
                            }
                            Command::Redraw
                        }
                        Some(Hit::Card(index)) if double => {
                            // Double-click applies the card.
                            state.activate(Hit::Card(index), &l, now);
                            state.activate(Hit::Button(Button::Apply), &l, now)
                        }
                        Some(Hit::BrowseRow(index)) if double => state.browse_choose(Some(index)),
                        Some(Hit::Cancel) if matches!(state.modal, Some(Modal::Help)) => {
                            state.modal = None;
                            Command::Redraw
                        }
                        Some(hit) => state.activate(hit, &l, now),
                    };
                    if !run_command(app, state, command)? {
                        return Ok((false, false));
                    }
                    state.hover = app.hit(state, &l, e.event_x, e.event_y);
                }
                _ => dirty = false,
            }
        }
        Event::SelectionNotify(e) => app.take_paste(&e, state)?,
        Event::ClientMessage(e) => {
            if e.type_ == app.atoms.protocols && e.data.as_data32()[0] == app.atoms.delete {
                return Ok((false, false));
            }
            dirty = false;
        }
        _ => dirty = false,
    }
    Ok((true, dirty))
}

/// The event loop. Returns when the window is closed.
pub fn run(app: &mut App, mut state: State) -> Result<(), String> {
    let state = &mut state;
    state.follow_selection(&layout(app.w, app.h));
    app.watch(state);
    app.request_previews(state);
    let mut dirty = true;
    let mut queued: Option<Event> = None;
    loop {
        loop {
            let event = match queued.take() {
                Some(event) => event,
                None => match app.c.poll_for_event().map_err(x)? {
                    Some(event) => event,
                    None => break,
                },
            };
            let (keep, changed) = handle_event(app, state, event)?;
            if !keep {
                return Ok(());
            }
            dirty |= changed;
        }
        app.wake.drain();
        let now = Instant::now();
        if app.collect_previews(state) {
            dirty = true;
        }
        while let Ok((serial, result, catalog)) = app.results.try_recv() {
            let l = layout(app.w, app.h);
            if state.finish(serial, result, catalog, theme::load(), &l, now) {
                app.watch(state);
                dirty = true;
            }
        }
        if let Some(busy) = &state.busy {
            let limit = backend::SLOW_DEADLINE + backend::QUICK_DEADLINE + Duration::from_secs(2);
            if now >= busy.started + limit {
                state.abandon();
                dirty = true;
            }
        }
        if app.watcher.drain() {
            app.debounce.note(now);
        }
        if state.busy.is_none() && app.debounce.take(now) {
            refresh_now(app, state);
            dirty = true;
        }
        if state.expire(now) {
            dirty = true;
        }
        if app.paste_until.is_some_and(|until| now >= until) {
            app.paste_until = None;
            state.set_message("The clipboard did not answer within 1 s.", true);
            dirty = true;
        }
        if let Some(Modal::Browse(browse)) = state.modal.as_mut() {
            browse.keep_visible(browse_rects(layout(app.w, app.h).modal()).rows);
        }
        if dirty {
            if app.drag.is_none() {
                let l = layout(app.w, app.h);
                state.hover = app.pointer.and_then(|(px, py)| app.hit(state, &l, px, py));
            }
            app.palette = state.palette;
            app.paint(state)?;
            app.flush()?;
            dirty = false;
        }
        app.request_previews(state);
        app.c.flush().map_err(x)?;
        // Painting can queue events without the socket being readable;
        // handle those before sleeping.
        if let Some(event) = app.c.poll_for_event().map_err(x)? {
            queued = Some(event);
            continue;
        }
        let now = Instant::now();
        let deadlines = [
            app.debounce.timeout(now),
            state
                .undo
                .as_ref()
                .map(|u| u.until.saturating_duration_since(now)),
            app.paste_until.map(|u| u.saturating_duration_since(now)),
            state.busy.as_ref().map(|b| {
                (b.started
                    + backend::SLOW_DEADLINE
                    + backend::QUICK_DEADLINE
                    + Duration::from_secs(2))
                .saturating_duration_since(now)
            }),
        ];
        wait_for_input(app, deadlines.into_iter().flatten().min());
    }
}

/// Block until the X server, a watched file or a worker has something to
/// say, or until `timeout` runs out. With nothing pending there is no
/// timeout at all: an idle window sleeps until something happens.
fn wait_for_input(app: &App, timeout: Option<Duration>) {
    use std::os::unix::io::AsRawFd;
    let mut fds =
        [app.c.stream().as_raw_fd(), app.watcher.fd(), app.wake.fd()].map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
    let ms = match timeout {
        // Round up, so the loop never wakes a millisecond early and spins.
        Some(t) => t.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32,
        None => -1,
    };
    // SAFETY: a live array of pollfd structures and its length.
    let _ = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
}
