# NIRU Theme Selector

**Part of [NIRUX - a minimal X11 desktop by Nicklas Rudolfsson](https://github.com/nirucon/nirux).**

A native Rust theme browser for NIRUX. It shows each theme as a small desktop
preview with its wallpaper, bar, window frame and palette. Selecting a card only
changes the preview; **Apply** uses the existing `niructl` theme path.

## Features

- Responsive grid of theme cards with wallpaper thumbnails and palette swatches,
  with "Themes 1–9 of 23" in the header and a scrollbar you can click or drag.
- Larger representative preview with a bar, focused window and launcher sample,
  the palette's hex values, the wallpaper's file name and resolution, the full
  error text of an invalid theme, and the other themes of the same family.
- Search: type to filter by name or id. Filter chips: **All · Built-in · User ·
  Hidden (n)**.
- Apply with Enter, a double-click or the Apply button. For 15 s afterwards,
  **Undo** (or Ctrl+Z) re-applies the theme that was active before.
- Import a theme TOML plus an optional PNG wallpaper, with paste, a cursor,
  an in-app **Browse…** list, and a check of both paths and the theme's name and
  palette before anything is imported. Remove user themes; hide/show bundled
  themes. Disabled buttons say why.
- A compact one-column layout when the window is tiled smaller than 900x620,
  down to 520x420.
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

## Keys

| Key | Action |
|---|---|
| typing | filter the grid by name or id |
| `Esc` | clear the filter; with no filter, close the window |
| `Tab` / `Shift+Tab` | move between cards, actions and filter chips |
| arrows, `PageUp`/`PageDown`, `Home`/`End` | choose a theme (only the preview changes) |
| `Enter` | apply the selected theme, or press the focused control |
| `Space` | press the focused control (adds a space once a filter is typed) |
| double-click | apply a card |
| `Ctrl+Z` | undo the last apply (15 s) |
| `Alt+A` / `Alt+I` / `Alt+H` / `Alt+D` | apply / import / hide-show / remove |
| `Alt+V` | show the hidden themes (the Hidden chip) |
| `Alt+R` | reload the catalogue |
| `Ctrl+Q` / `Alt+Q` | close |
| `F1` | help |

Single letters type into the filter, so the shortcuts of 0.1 (`A`, `I`, `H`,
`D`, `V`, `R`, `Q`) moved to `Alt`+letter. Shortcuts match the key's base
symbol, so they work on every layout. Text input follows the keyboard layout:
Shift, AltGr (`$` and `@` on a Swedish keyboard), Caps Lock and the dead keys,
which type their literal accent (`~`, `^`, `` ` ``, `´`, `¨`). The key table is
read again when the layout changes.

## Import and removal

Import accepts the existing NIRUX TOML theme format. An optional PNG wallpaper is
copied beside the other NIRUX theme wallpapers and recorded with the user theme;
`niructl theme-import` accepts PNG only, so another format is refused in the form
with that reason. In the fields: `Ctrl+V` or `Shift+Insert` pastes the
clipboard, `Ctrl+B` opens **Browse…** (it starts in `~/Downloads` and shows only
folders and `.toml` or `.png` files), `Ctrl+R` toggles replacing a user theme
with the same id, `Enter` imports. User themes can be removed; bundled themes can
only be hidden. Removing or hiding the active theme is refused until another
theme is applied. Imported theme files are data and are never executed.

## Event model

The window does nothing while idle: it sleeps until the X server, a watched file
or one of its own worker threads has something to say, and starts no process.

- Apply, import, hide and remove run `niructl` on one worker thread, one action
  at a time, each with a deadline (30 s for apply and import, 5 s otherwise;
  a program that does not answer is killed). The window keeps painting and shows
  "Applying Niru Nord…" with the actions disabled. A late answer never
  overwrites a newer state.
- External changes (`niructl theme X` in a terminal, the hidden list, a new user
  theme, a theme wallpaper) are noticed through inotify on the files `niructl`
  changes, debounced by 300 ms into one catalogue read. Focusing the window also
  reads it again; `Alt+R` reloads by hand.
- Thumbnails decode on two workers; pictures over 16 MP decode one at a time,
  which bounds the first-run peak memory with 8K wallpapers.

## Build

```sh
cargo build --release --locked
cargo test --locked
sh tests/cli-smoke.sh target/release/niruthemeselector
cargo test --locked --test xvfb -- --ignored   # needs Xvfb, xdotool, xwd, setxkbmap, xclip
```

The minimum Rust version is 1.85 (Debian 13); CI builds and tests with it.

Dependencies are `x11rb`, `ab_glyph`, `serde_json`, `toml` and `image`; no GTK, Qt,
Electron or WebView runtime is required.

MIT. Copyright 2026 Nicklas Rudolfsson.
