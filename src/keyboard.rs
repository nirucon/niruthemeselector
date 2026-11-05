//! Keysyms with their level: Shift, AltGr, Caps Lock and the dead keys.
//!
//! Ported from NIRU Launcher's `keyboard_level`, `apply_caps` and
//! `keyboard_symbol` (`nirucon/nirulauncher`), with the core-map column fix
//! NIRU Wallpaper 0.2.0 and NIRUSHOT 0.5.0 use: X's core keyboard map lists a
//! key's symbols as group 1 level 1-2, group 2 level 1-2, then group 1 level
//! 3-4 (XKB protocol, "core keyboard mapping"). AltGr (level 3) is therefore
//! the fifth symbol when the server reports more than four per key; only a
//! shorter list (a test, or an old server) has it third.
//!
//! The key table is kept whole (every level) and read again after a
//! `MappingNotify`, which X sends for `setxkbmap` and every other XKB map
//! change.

use x11rb::{
    connection::Connection,
    protocol::xproto::{ConnectionExt as _, KeyButMask},
};

/// Keyboard level: base, Shift, AltGr (MOD5), then Shift+AltGr.
pub fn keyboard_level(state: KeyButMask) -> usize {
    let shift = state.contains(KeyButMask::SHIFT);
    let altgr = state.contains(KeyButMask::MOD5);
    match (shift, altgr) {
        (false, false) => 0,
        (true, false) => 1,
        (false, true) => 2,
        (true, true) => 3,
    }
}

/// X applies Caps Lock to alphabetic keysyms, including Latin-1 letters.
pub fn apply_caps(sym: u32, lock: bool) -> u32 {
    if !lock {
        return sym;
    }
    if (0x61..=0x7a).contains(&sym) || (0xe0..=0xfe).contains(&sym) && sym != 0xf7 {
        return sym.saturating_sub(0x20);
    }
    if (0x41..=0x5a).contains(&sym) || (0xc0..=0xde).contains(&sym) && sym != 0xd7 {
        return sym + 0x20;
    }
    sym
}

/// The keysym one key produces with the given modifiers.
pub fn keyboard_symbol(syms: &[u32], state: KeyButMask) -> u32 {
    let level = keyboard_level(state);
    // Index of each level in the core map; see the module comment.
    let index = if syms.len() > 4 {
        [0, 1, 4, 5][level]
    } else {
        level
    };
    let sym = syms
        .get(index)
        .filter(|s| **s != 0 && **s != 0xfe03)
        .or_else(|| syms.get(level % 2).filter(|s| **s != 0))
        .or_else(|| syms.first())
        .copied()
        .unwrap_or(0);
    apply_caps(sym, state.contains(KeyButMask::LOCK))
}

/// The keysym a shortcut compares against: the key's base symbol in lower
/// case, so Shift, AltGr, Caps Lock and the layout's levels never change
/// which shortcut a letter is.
pub fn shortcut(sym: u32) -> u32 {
    if (0x41..=0x5a).contains(&sym) || (0xc0..=0xde).contains(&sym) && sym != 0xd7 {
        sym + 0x20
    } else {
        sym
    }
}

/// The text one keysym types into a field, or `None` for a key that is not
/// text (a modifier, an arrow, a function key).
///
/// Latin-1 0x20-0xff maps to its character, Unicode keysyms to theirs, and
/// the five dead keys of the Latin layouts type their literal accent at once:
/// a path field has no use for composed letters, and `~` (AltGr + the
/// dead-tilde key on a Swedish keyboard) must be typeable for `~/`.
pub fn text_char(sym: u32) -> Option<char> {
    match sym {
        0x20..=0x7e | 0xa0..=0xff => char::from_u32(sym),
        0x0100_0100..=0x0110_ffff => char::from_u32(sym - 0x0100_0000),
        0xfe50 => Some('`'),      // dead_grave
        0xfe51 => Some('\u{b4}'), // dead_acute
        0xfe52 => Some('^'),      // dead_circumflex
        0xfe53 => Some('~'),      // dead_tilde
        0xfe57 => Some('\u{a8}'), // dead_diaeresis
        // Keypad keys that produce text.
        0xffaa => Some('*'),
        0xffab => Some('+'),
        0xffad => Some('-'),
        0xffae => Some('.'),
        0xffaf => Some('/'),
        0xffb0..=0xffb9 => char::from_u32(sym - 0xffb0 + u32::from(b'0')),
        _ => None,
    }
    .filter(|c| !c.is_control())
}

/// The core keyboard map, fetched once and again after `MappingNotify`
/// instead of one round trip per key press.
#[derive(Clone, Debug, Default)]
pub struct Keys {
    min: u8,
    per: usize,
    syms: Vec<u32>,
}

impl Keys {
    pub fn load(c: &impl Connection) -> Result<Self, String> {
        let setup = c.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let map = c
            .get_keyboard_mapping(min, max.saturating_sub(min) + 1)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        Ok(Self::from_table(
            min,
            usize::from(map.keysyms_per_keycode),
            map.keysyms,
        ))
    }

    pub fn from_table(min: u8, per: usize, syms: Vec<u32>) -> Self {
        Self { min, per, syms }
    }

    /// Every symbol of one key, in core-map order.
    pub fn syms(&self, code: u8) -> &[u32] {
        let Some(index) = code.checked_sub(self.min) else {
            return &[];
        };
        let start = usize::from(index) * self.per;
        self.syms.get(start..start + self.per).unwrap_or(&[])
    }

    /// The keysym a key produces with these modifiers.
    pub fn sym(&self, code: u8, state: KeyButMask) -> u32 {
        keyboard_symbol(self.syms(code), state)
    }

    /// The key's base symbol, for shortcuts.
    pub fn base(&self, code: u8) -> u32 {
        shortcut(self.syms(code).first().copied().unwrap_or(0))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// The parts of `setxkbmap se`'s core map (Xvfb, 7 symbols per key) that
    /// the T2 path needs, as `xmodmap -pke` prints them.
    pub fn swedish() -> Keys {
        const NONE: u32 = 0;
        let mut syms = vec![NONE; 256 * 7];
        let mut put = |code: usize, row: &[u32]| {
            let start = (code - 8) * 7;
            syms[start..start + row.len()].copy_from_slice(row);
        };
        put(11, &[0x32, 0x22, 0x32, 0x22, 0x40, 0xad3, 0x40]); // 2 " @
        put(13, &[0x34, 0xa4, 0x34, 0xa4, 0x24, 0xbc, 0x24]); // 4 ¤ $
        put(16, &[0x37, 0x2f, 0x37, 0x2f, 0x7b, 0x8c6, 0x7b]); // 7 / {
        put(28, &[0x74, 0x54, 0x74, 0x54, 0x3fe, 0x3de, 0x3fe]); // t T
        put(26, &[0x65, 0x45, 0x65, 0x45, 0x20ac, 0xa2, 0x20ac]); // e E €
        put(58, &[0x6d, 0x4d, 0x6d, 0x4d, 0xb5, 0xba, 0xb5]); // m M µ
        put(38, &[0x61, 0x41, 0x61, 0x41, 0xaa, 0xba, 0xaa]); // a A ª
        put(57, &[0x6e, 0x4e, 0x6e, 0x4e, 0x2019, 0x2018, 0x2019]); // n N
        put(53, &[0x78, 0x58, 0x78, 0x58, 0xbb, 0x3e, 0xbb]); // x X »
        put(34, &[0xe5, 0xc5, 0xe5, 0xc5, 0xfe57, 0xfe5a, 0xfe57]); // å Å
        put(
            35,
            &[0xfe57, 0xfe52, 0xfe57, 0xfe52, 0xfe53, 0xfe5a, 0xfe53],
        ); // ¨ ^ ~
        put(60, &[0x2e, 0x3a, 0x2e, 0x3a, 0xb7, 0xf7, 0xb7]); // . :
        put(32, &[0x6f, 0x4f, 0x6f, 0x4f, 0x153, 0x152, 0x153]); // o O
        put(46, &[0x6c, 0x4c, 0x6c, 0x4c, 0x3a6, 0x26, 0x3a6]); // l L
        Keys::from_table(8, 7, syms)
    }

    fn typed(keys: &Keys, presses: &[(u8, KeyButMask)]) -> String {
        presses
            .iter()
            .filter_map(|(code, state)| text_char(keys.sym(*code, *state)))
            .collect()
    }

    #[test]
    fn the_level_follows_shift_and_altgr() {
        assert_eq!(keyboard_level(KeyButMask::default()), 0);
        assert_eq!(keyboard_level(KeyButMask::SHIFT), 1);
        assert_eq!(keyboard_level(KeyButMask::MOD5), 2);
        assert_eq!(keyboard_level(KeyButMask::SHIFT | KeyButMask::MOD5), 3);
    }

    #[test]
    fn altgr_and_caps_produce_the_expected_symbols() {
        let core = [0x32, 0x22, 0x32, 0x22, 0x40, 0xad3];
        assert_eq!(keyboard_symbol(&core, KeyButMask::default()), 0x32);
        assert_eq!(keyboard_symbol(&core, KeyButMask::SHIFT), 0x22);
        assert_eq!(keyboard_symbol(&core, KeyButMask::MOD5), 0x40, "AltGr+2");
        assert_eq!(
            keyboard_symbol(&core, KeyButMask::SHIFT | KeyButMask::MOD5),
            0xad3
        );
        // The launcher's four-symbol form still works.
        let short = [0x32, 0x22, 0x40, 0x32];
        assert_eq!(keyboard_symbol(&short, KeyButMask::MOD5), 0x40);
        // A key without an AltGr level falls back to its base symbols.
        let plain = [0x61, 0x41, 0x61, 0x41];
        assert_eq!(keyboard_symbol(&plain, KeyButMask::MOD5), 0x61);
        assert_eq!(keyboard_symbol(&plain, KeyButMask::LOCK), 0x41, "Caps");
        assert_eq!(apply_caps(0xe5, true), 0xc5, "Caps Lock applies to å");
        assert_eq!(apply_caps(0x40, true), 0x40, "Caps Lock leaves @ alone");
    }

    #[test]
    fn dead_keys_type_their_literal_accent() {
        assert_eq!(text_char(0xfe53), Some('~'));
        assert_eq!(text_char(0xfe52), Some('^'));
        assert_eq!(text_char(0xfe50), Some('`'));
        assert_eq!(text_char(0xfe51), Some('´'));
        assert_eq!(text_char(0xfe57), Some('¨'));
        assert_eq!(text_char(0xe5), Some('å'));
        assert_eq!(text_char(0xc5), Some('Å'));
        assert_eq!(text_char(0x20ac), None, "a legacy keysym is not Latin-1");
        assert_eq!(text_char(0x0100_20ac), Some('€'));
        assert_eq!(text_char(0xffe1), None, "Shift is not text");
        assert_eq!(text_char(0xff51), None, "Left is not text");
    }

    #[test]
    fn a_swedish_keymap_types_the_documented_path() {
        let keys = swedish();
        let n = KeyButMask::default();
        let s = KeyButMask::SHIFT;
        let g = KeyButMask::MOD5;
        let path = typed(
            &keys,
            &[
                (35, g), // AltGr + dead diaeresis key: dead_tilde → ~
                (16, s), // Shift+7: /
                (28, s), // T
                (26, n),
                (58, n),
                (38, n),
                (57, n),
                (16, s), // /
                (13, g), // AltGr+4: $
                (53, n), // x
                (11, g), // AltGr+2: @
                (34, s), // Shift+å: Å
                (60, n), // .
                (28, n),
                (32, n),
                (58, n),
                (46, n),
            ],
        );
        assert_eq!(path, "~/Teman/$x@Å.toml");
        // Caps Lock shifts letters, including å, but not digits.
        assert_eq!(text_char(keys.sym(34, KeyButMask::LOCK)), Some('Å'));
        assert_eq!(text_char(keys.sym(13, KeyButMask::LOCK)), Some('4'));
    }

    #[test]
    fn shortcuts_use_the_base_symbol() {
        let keys = swedish();
        assert_eq!(keys.base(38), u32::from(b'a'));
        assert_eq!(keys.base(34), 0xe5);
        assert_eq!(shortcut(0x41), 0x61);
        assert_eq!(shortcut(0xff09), 0xff09, "Tab is not a letter");
        assert!(keys.syms(7).is_empty(), "below min_keycode");
    }
}
