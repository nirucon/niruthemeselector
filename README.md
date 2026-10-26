# NIRU Theme Selector

**Part of [NIRUX - a minimal X11 desktop by Nicklas Rudolfsson](https://github.com/nirucon/nirux).**

A native Rust theme browser for NIRUX. It shows each theme as a small desktop
preview with its wallpaper, bar, window frame and palette. Selecting a card only
changes the preview; **Apply** uses the existing `niructl` theme path.

## Features

- Responsive grid of theme cards with wallpaper thumbnails and palette swatches.
- Larger representative preview with a bar, focused window and launcher sample.
- Active, selected, hidden and invalid states are distinct.
- Full keyboard use with Tab, arrows, Enter/Space and Escape.
- Apply, import a theme TOML plus optional PNG wallpaper, remove user themes and
  hide/show bundled themes.
- Honest wallpaper fallback: a theme without its image uses its background colour.
- Background thumbnail decoding and a bounded disk/memory cache. No daemon and
  no network access while browsing.
- Existing NIRUX settings, launcher and WM theme path remain the source of truth.

## Usage

```sh
niruthemeselector              # open the window
niruthemeselector status       # active theme and catalogue
niruthemeselector list         # available themes
niruthemeselector --version | --help
```

Keys: `Tab`/`Shift+Tab` focus, arrows choose a theme, `Enter`/`Space` activate,
`A` apply, `I` import, `H` hide/show, `D` remove a user theme, `V` show/hide
hidden themes, `R` reload, `F1` help, `Esc` close.

## Import and removal

Import accepts the existing NIRUX TOML theme format. An optional PNG wallpaper is
copied beside the other NIRUX theme wallpapers and recorded with the user theme.
User themes can be removed; bundled themes can only be hidden. Removing the
active theme is refused until another theme is applied. Imported theme files are
data and are never executed.

## Build

```sh
cargo build --release --locked
cargo test --locked
sh tests/cli-smoke.sh target/release/niruthemeselector
```

Dependencies are `x11rb`, `ab_glyph`, `serde_json` and `image`; no GTK, Qt,
Electron or WebView runtime is required.

MIT. Copyright 2026 Nicklas Rudolfsson.
