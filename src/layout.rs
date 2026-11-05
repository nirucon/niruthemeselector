//! Where everything goes, for the window's real size.
//!
//! Pure geometry, so hit testing and painting agree and the compact layout
//! can be tested without an X server. Three forms:
//!
//! * **normal** (900x620 and up): the grid on the left, the preview panel on
//!   the right, as in 0.1;
//! * **compact** (down to 520x420, for a tiled window): one column, with the
//!   preview collapsed to a strip above the grid;
//! * **too small**: only "Make the window larger".

use x11rb::protocol::xproto::Rectangle;

pub const NORMAL_W: u16 = 900;
pub const NORMAL_H: u16 = 620;
pub const COMPACT_W: u16 = 520;
pub const COMPACT_H: u16 = 420;

pub const PAD: i16 = 20;
pub const CARD_H: i16 = 164;
pub const GAP: i16 = 12;
const HEADER: i16 = 76;
const HEADER_COMPACT: i16 = 84;
const STRIP_H: i16 = 52;
const FOOTER: i16 = 56;
const FOOTER_COMPACT: i16 = 44;
const ACTIONS: i16 = 62;
const ACTIONS_COMPACT: i16 = 36;
const PANEL_W: i16 = 300;
/// Room at the grid's right edge for the scrollbar.
const SCROLL_ROOM: i16 = 14;
/// Filter chips: All, Built-in, User, Hidden (n).
const CHIP_W: [i16; 4] = [44, 76, 56, 104];
const CHIP_GAP: i16 = 6;
const CHIPS_TOTAL: i16 = 44 + 76 + 56 + 104 + 3 * CHIP_GAP;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Compact,
    TooSmall,
}

/// The five action buttons, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Apply,
    Import,
    Remove,
    Hide,
    Reload,
}

pub const BUTTONS: [Button; 5] = [
    Button::Apply,
    Button::Import,
    Button::Remove,
    Button::Hide,
    Button::Reload,
];

/// Filter chips above the grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Chip {
    #[default]
    All,
    Builtin,
    User,
    Hidden,
}

pub const CHIPS: [Chip; 4] = [Chip::All, Chip::Builtin, Chip::User, Chip::Hidden];

#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub w: u16,
    pub h: u16,
    pub mode: Mode,
    pub title_y: i16,
    pub subtitle_y: i16,
    /// Width available for the title and the subtitle.
    pub header_text_w: u16,
    pub filter: Rectangle,
    pub chips: [Rectangle; 4],
    pub grid: Rectangle,
    /// The scrollbar's track, inside the grid's right edge.
    pub track: Rectangle,
    /// The preview panel (normal) or strip (compact).
    pub panel: Rectangle,
    /// The line explaining a disabled button, when the layout has one;
    /// compact layouts show it in the message line instead.
    pub reason_y: Option<i16>,
    pub buttons: [Rectangle; 5],
    pub message: Rectangle,
    pub hints_y: i16,
    pub card_w: u16,
    pub cols: usize,
    pub rows: usize,
}

fn rect(x: i16, y: i16, w: i16, h: i16) -> Rectangle {
    Rectangle {
        x,
        y,
        width: w.max(0) as u16,
        height: h.max(0) as u16,
    }
}

pub fn layout(w: u16, h: u16) -> Layout {
    let mode = if w >= NORMAL_W && h >= NORMAL_H {
        Mode::Normal
    } else if w >= COMPACT_W && h >= COMPACT_H {
        Mode::Compact
    } else {
        Mode::TooSmall
    };
    let width = i16::try_from(w).unwrap_or(i16::MAX);
    let height = i16::try_from(h).unwrap_or(i16::MAX);
    let inner = width - PAD * 2;
    let (header, title_y, subtitle_y, filter, chips_x, chips_y) = match mode {
        Mode::Normal | Mode::TooSmall => {
            let x = width - PAD - CHIPS_TOTAL;
            (HEADER, 12, 42, rect(x, 8, CHIPS_TOTAL, 24), x, 40)
        }
        Mode::Compact => {
            let x = (width - PAD - CHIPS_TOTAL).max(PAD + 240);
            (
                HEADER_COMPACT,
                8,
                34,
                rect(x, 6, width - PAD - x, 24),
                PAD,
                56,
            )
        }
    };
    let mut chips = [rect(0, 0, 0, 0); 4];
    let mut x = chips_x;
    for (chip, width) in chips.iter_mut().zip(CHIP_W) {
        *chip = rect(x, chips_y, width, 22);
        x += width + CHIP_GAP;
    }
    let header_text_w = match mode {
        Mode::Compact => (filter.x - PAD - 12).max(0) as u16,
        _ => (chips_x - PAD - 12).max(0) as u16,
    };
    let (grid, panel, actions_y, button_y, reason_y, footer_y, hints_dy) = match mode {
        Mode::Compact => {
            let panel = rect(PAD, header, inner, STRIP_H);
            let top = header + STRIP_H + 8;
            let footer_y = height - FOOTER_COMPACT;
            let actions_y = footer_y - ACTIONS_COMPACT;
            let grid = rect(PAD, top, inner, actions_y - top);
            (
                grid,
                panel,
                actions_y,
                actions_y + 4,
                None,
                footer_y + 2,
                20,
            )
        }
        _ => {
            let grid_h = height - header - FOOTER - ACTIONS - 16;
            let grid = rect(PAD, header, inner - PANEL_W - 16, grid_h);
            let panel = rect(width - PAD - PANEL_W, header, PANEL_W, grid_h);
            let actions_y = height - FOOTER - ACTIONS;
            let footer_y = height - FOOTER;
            (
                grid,
                panel,
                actions_y,
                actions_y + 20,
                Some(actions_y + 2),
                footer_y,
                20,
            )
        }
    };
    let _ = actions_y;
    let mut buttons = [rect(0, 0, 0, 0); 5];
    let button_w = (inner - 4 * 4) / 5;
    for (index, button) in buttons.iter_mut().enumerate() {
        *button = rect(PAD + index as i16 * (button_w + 4), button_y, button_w, 27);
    }
    let area = grid.width as i16 - SCROLL_ROOM;
    let cols: usize = if area >= 740 {
        3
    } else if area >= 330 {
        2
    } else {
        1
    };
    let card_w = ((area - GAP * (cols as i16 + 1)) / cols as i16).max(0) as u16;
    let rows = (grid.height / (CARD_H as u16 + GAP as u16)).max(1) as usize;
    let track = rect(
        grid.x + grid.width as i16 - 9,
        grid.y + GAP,
        6,
        grid.height as i16 - GAP * 2,
    );
    Layout {
        w,
        h,
        mode,
        title_y,
        subtitle_y,
        header_text_w,
        filter,
        chips,
        grid,
        track,
        panel,
        reason_y,
        buttons,
        message: rect(PAD, footer_y, inner, 18),
        hints_y: footer_y + hints_dy,
        card_w,
        cols,
        rows,
    }
}

impl Layout {
    /// The rectangle of the card shown at `screen_index` (0 = the first card
    /// in view).
    pub fn card(&self, screen_index: usize) -> Rectangle {
        let col = screen_index % self.cols;
        let row = screen_index / self.cols;
        Rectangle {
            x: self.grid.x + GAP + col as i16 * (self.card_w as i16 + GAP),
            y: self.grid.y + GAP + row as i16 * (CARD_H + GAP),
            width: self.card_w,
            height: CARD_H as u16,
        }
    }

    pub fn button(&self, button: Button) -> Rectangle {
        self.buttons[BUTTONS.iter().position(|b| *b == button).unwrap_or(0)]
    }

    pub fn chip(&self, chip: Chip) -> Rectangle {
        self.chips[CHIPS.iter().position(|c| *c == chip).unwrap_or(0)]
    }

    pub fn page(&self) -> usize {
        self.cols * self.rows
    }

    /// The dialog box for import, browse, remove and help.
    pub fn modal(&self) -> Rectangle {
        let width = 720.min(self.w.saturating_sub(40));
        let height = 400.min(self.h.saturating_sub(24));
        Rectangle {
            x: (self.w as i16 - width as i16) / 2,
            y: (self.h as i16 - height as i16) / 2,
            width,
            height,
        }
    }
}

pub fn contains(rect: Rectangle, x: i16, y: i16) -> bool {
    x >= rect.x
        && i32::from(x) < i32::from(rect.x) + i32::from(rect.width)
        && y >= rect.y
        && i32::from(y) < i32::from(rect.y) + i32::from(rect.height)
}

#[cfg(test)]
pub fn intersects(a: Rectangle, b: Rectangle) -> bool {
    let (ax2, ay2) = (
        i32::from(a.x) + i32::from(a.width),
        i32::from(a.y) + i32::from(a.height),
    );
    let (bx2, by2) = (
        i32::from(b.x) + i32::from(b.width),
        i32::from(b.y) + i32::from(b.height),
    );
    i32::from(a.x) < bx2 && i32::from(b.x) < ax2 && i32::from(a.y) < by2 && i32::from(b.y) < ay2
}

#[cfg(test)]
pub fn inside(inner: Rectangle, outer: Rectangle) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && i32::from(inner.x) + i32::from(inner.width)
            <= i32::from(outer.x) + i32::from(outer.width)
        && i32::from(inner.y) + i32::from(inner.height)
            <= i32::from(outer.y) + i32::from(outer.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every region of a layout, named, for the collision checks.
    fn regions(l: &Layout) -> Vec<(String, Rectangle)> {
        let mut out = vec![
            (
                "title".to_string(),
                rect(
                    PAD,
                    l.title_y,
                    l.header_text_w as i16,
                    l.subtitle_y - l.title_y + 16,
                ),
            ),
            ("filter".into(), l.filter),
            ("grid".into(), l.grid),
            ("panel".into(), l.panel),
            ("message".into(), l.message),
            (
                "hints".into(),
                rect(PAD, l.hints_y, l.w as i16 - PAD * 2, 14),
            ),
        ];
        if let Some(y) = l.reason_y {
            out.push(("reason".into(), rect(PAD, y, l.w as i16 - PAD * 2, 16)));
        }
        for (index, chip) in l.chips.iter().enumerate() {
            out.push((format!("chip{index}"), *chip));
        }
        for (index, button) in l.buttons.iter().enumerate() {
            out.push((format!("button{index}"), *button));
        }
        out
    }

    fn assert_clean(w: u16, h: u16, mode: Mode) {
        let l = layout(w, h);
        assert_eq!(l.mode, mode, "{w}x{h}");
        let window = rect(0, 0, w as i16, h as i16);
        let regions = regions(&l);
        for (name, r) in &regions {
            assert!(r.width > 0 && r.height > 0, "{w}x{h}: {name} is empty");
            assert!(
                inside(*r, window),
                "{w}x{h}: {name} {r:?} leaves the window"
            );
        }
        for (i, (a, ra)) in regions.iter().enumerate() {
            for (b, rb) in &regions[i + 1..] {
                assert!(
                    !intersects(*ra, *rb),
                    "{w}x{h}: {a} {ra:?} overlaps {b} {rb:?}"
                );
            }
        }
        // Every card in view and the scrollbar stay inside the grid, and
        // the cards never run under the scrollbar.
        assert!(inside(l.track, l.grid));
        assert!(l.rows >= 1 && l.cols >= 1);
        for index in 0..l.page() {
            let card = l.card(index);
            assert!(inside(card, l.grid), "{w}x{h}: card {index} {card:?}");
            assert!(
                !intersects(card, l.track),
                "{w}x{h}: card {index} under the bar"
            );
            for other in index + 1..l.page() {
                assert!(!intersects(card, l.card(other)));
            }
        }
        assert!(inside(l.modal(), window), "{w}x{h}: dialog");
    }

    #[test]
    fn the_normal_layout_keeps_three_columns_at_the_default_size() {
        assert_clean(1120, 760, Mode::Normal);
        let l = layout(1120, 760);
        assert_eq!((l.cols, l.rows), (3, 3), "the 0.1 grid of nine cards");
        assert_clean(900, 620, Mode::Normal);
        assert_clean(2560, 1440, Mode::Normal);
    }

    #[test]
    fn the_compact_layout_has_no_collisions_down_to_520x420() {
        assert_clean(520, 420, Mode::Compact);
        assert_clean(899, 619, Mode::Compact);
        assert_clean(899, 1200, Mode::Compact);
        assert_clean(1400, 619, Mode::Compact);
        assert_clean(640, 480, Mode::Compact);
        let l = layout(520, 420);
        assert!(
            l.panel.y + l.panel.height as i16 <= l.grid.y,
            "strip above the grid"
        );
        assert_eq!(l.rows, 1);
    }

    #[test]
    fn below_520x420_the_window_asks_to_be_larger() {
        assert_eq!(layout(519, 600).mode, Mode::TooSmall);
        assert_eq!(layout(800, 419).mode, Mode::TooSmall);
        assert_eq!(layout(10, 10).mode, Mode::TooSmall);
    }
}
