# Agent assignment: NIRUTHEMESELECTOR 0.2.0, a one-shot UX, bug and efficiency update

## 0. Quick start

1. Read this file, `README.md`, `src/window.rs`, `src/backend.rs` and `src/thumbs.rs`.
   The audit baseline is `8b1e2f0` (0.1.2, pinned by NIRUX 0.12.199). Revalidate each
   item against current HEAD and skip items that are already fixed, noting that you did.
2. Implement **T1–T9, then U1–U7 in order**. Commit per item or per small group. Keep
   `cargo fmt --check`, `clippy -D warnings`, tests and `tests/cli-smoke.sh` green.
3. Pass the gates in §4, release in §5 and report in §6.

This is an **execution** assignment. Do not stop after a plan, a branch or a draft
PR. Make sensible decisions, document them, and continue without routine questions.
Only ask if you hit a genuine external blocker or a real risk of losing the owner's data.

## 1. Product invariants

- `niructl` stays the source of truth: `theme-catalog`, `theme`, `theme-import`,
  `theme-remove`, `theme-hide` and `theme-show`. The app owns no theme store. Do not
  change `niructl` unless an item explicitly allows it.
- Selecting a card only changes the preview. Only **Apply** changes the desktop.
- Imported theme files are data and are never executed. Bundled themes can be hidden,
  not removed. The active theme cannot be removed.
- No GTK/Qt and no network. Keep `rust-version = 1.85` and `--locked` builds working
  **with rustc 1.85**. Debian 13 ships 1.85.
- The look stays, except where an item changes it on purpose. The UI text is English.

## 2. Bugs (verified on niruxwork 2026-11-04)

### T1: The locked build fails on the declared Rust version (high)
`Cargo.lock` has `image 0.25.10`, which requires rustc 1.88, but `rust-version = "1.85"`.
`cargo build --locked` fails on Debian 13 (rustc 1.85.1). CI uses stable, so it does
not notice. **Fix:** `cargo update image --precise 0.25.9` (verified to build on 1.85),
and add an MSRV CI job (`dtolnay/rust-toolchain@1.85`, `cargo build --locked`).

### T2: Text input ignores AltGr, Caps Lock and dead keys (high)
`Keys` keeps only levels 0 and 1. With `setxkbmap se`, AltGr+4 typed `4` instead of `$`
and AltGr+2 typed `2` instead of `@`. `~` (AltGr + the dead-tilde key) cannot be typed
at all, even though the documented `~/` path shortcut depends on it. The keymap is read
once and never refreshed.
**Fix:** port the tested keyboard handling from `nirucon/nirulauncher` `src/main.rs`:
`keyboard_level` (Shift/MOD5), `apply_caps` and `keyboard_symbol`, with their tests. Keep
all levels, and refresh on `MappingNotify` and XKB map changes. Map Latin-1 0x20–0xff to
chars. Treat the dead keys (tilde, circumflex, grave, acute, diaeresis) as their literal
characters. Shortcut letters (A, I, H, D, V, R, Q) still match on the base keysym.
**Done when:** a keymap fixture unit test and an Xvfb test with `setxkbmap se` enter
`~/Teman/$x@Å.toml` exactly.

### T3: The mouse wheel cannot reach the bottom of the grid (high)
After every loop iteration, `run()` calls `state.ensure_visible()`. That pulls the scroll
position back so that the *selected* card stays visible. With 23 themes and 9 visible,
12 wheel notches stopped at row 3, so themes 16–23 (Erik Satie variants, Evert Taube,
Hackerman, Piaf) **could not be reached with the wheel**. `scroll += 1` also has no upper
bound.
**Fix:** follow the selection only when the selection changes through the keyboard,
selection, refresh or import. Clamp the scroll to `0..=max_row_offset`. Wheel scrolling
moves the view, not the selection.

### T4: Apply, import, hide and remove freeze the window
These actions call `niructl` synchronously without a deadline. `niructl theme` composes
wallpapers with ffmpeg and reloads the bar and WM, which can take seconds. During that
time the window does not repaint, input queues up, and a second click can apply twice.
**Fix:**
- Run the actions on one worker thread, one action at a time.
- Show a busy state: "Applying Niru Nord…" with the buttons disabled, and repaint.
- Bound each call with a deadline using the same pattern as nirubar's `probe::run`
  (non-blocking reads, kill and reap). Use 30 s for apply and import, 5 s for the rest.
- A late result must not overwrite a newer state.

### T5: The window polls while idle
`wait_for_input` wakes every 250 ms. Every 5 s the app spawns `niructl theme-catalog` and
reads the palette file. **Fix:**
- Wake on the X socket and on a pipe/eventfd that the thumbnail workers and the action
  worker signal. Block indefinitely when nothing is pending.
- Notice external theme changes through inotify on the files `niructl` changes. Find them
  from the `niructl` source: the theme config file, the user theme dir and the hidden list.
  Debounce by 300 ms, then run one catalog refresh. Also refresh on `FocusIn`.
- Keep `R` for a manual reload.

**Done when:** an idle open window starts 0 processes over 60 s, and a `niructl theme X`
run from a terminal shows up in the window within 1 s.

### T6: Thumbnail eviction is arbitrary
At 64 entries, `collect_thumbs` evicts `HashMap::keys().next()`, which can be a visible
thumbnail, so it reloads and flickers. **Fix:** use LRU by last paint and never evict a
thumbnail that is visible or selected.

### T7: Decode memory is unbounded in practice
`MAX_PIXELS * 5` allows about 1 GB of allocation, and there are two workers. 8K wallpapers
from `nirux-wallpapers` can push peak RSS to hundreds of MB on the first run, before the
disk cache exists. **Fix:** measure peak RSS while decoding several 8K PNG/JPEG files at
once. Bound it, for example with one worker for images over 16 MP and a lower
`max_alloc`, and use JPEG DCT downscaled decoding if `image` allows it on the 1.85
baseline. Target a peak RSS below 300 MB, documented with the measured number.

### T8: The close handler interns an atom on every ClientMessage
It also does not check `data[0] == WM_DELETE_WINDOW`. **Fix:** intern both atoms once in
`App::open` and compare both.

### T9: The window breaks when it is tiled smaller than its minimum size
`ConfigureNotify` clamps `app.w`/`app.h` to 900x620 even when niruwm tiles the window
smaller, so the content is cut off. **Fix:** paint for the real size. Below the current
minimum, use one column with the preview panel above the grid and collapsed to a strip,
down to about 520x420. Below that, show "Make the window larger". Set
`_NET_WM_WINDOW_TYPE_DIALOG` if niruwm then floats the window. Check niruwm's rules and do
not change niruwm.

## 3. UX and functionality

### U1: Show where you are in the list
Show "Themes 1–9 of 23" in the header and a thin scrollbar on the grid that can be clicked
or dragged. Add PageUp/PageDown/Home/End. The header should show the active theme's
**name**, not its id.

### U2: Search and filters
Typing a printable key while the grid has focus starts a filter on name and id. Esc
clears it first and closes the window only on the second press. Add filter chips:
**All · Built-in · User · Hidden (n)**. "Show hidden" becomes the Hidden chip with a count.
Move the single-letter shortcuts (A, I, H, D, V, R) to Alt+letter or keep them only when
the filter is empty. Choose one, keep it consistent, and update the footer and F1 help.

### U3: Applying is obvious and can be undone
Double-clicking a card applies it, and Enter on a focused card applies it, so the footer's
"Enter apply" becomes true. After a successful apply, show "Applied Niru Nord. **Undo**"
for 15 s. Undo re-applies the previous active id through the same worker, so no `niructl`
change is needed.

### U4: A better import
- Fields support paste with Ctrl+V and Shift+Insert. Read CLIPBOARD as UTF8_STRING through
  a normal X selection conversion with a 1 s deadline.
- Add cursor movement with Left/Right/Home/End.
- Add a **Browse…** list: a simple in-app directory list that starts in `~/Downloads`
  and can switch to `~`. It shows only directories and `.toml` files, or images for the
  wallpaper field.
- Validate as the user types: show "✓ found" or "✗ not found" per path, and show the parsed
  theme name and palette from the TOML before import (read-only parse with `toml`).
- Accept the wallpaper formats that `niructl theme-import` actually accepts. Check the
  source. If it accepts only PNG, keep the label "PNG" and say so in the error message
  instead of failing later.

### U5: Use the preview panel
Show the full error text for invalid themes (wrapped), the wallpaper file name and
resolution, the hex values next to the swatches, and "Built-in · hidden" style status.
Show other members of the same family (a shared name prefix such as "Niru Carl Larsson …")
as small clickable chips.

### U6: Explain disabled actions
When a disabled button is hovered or focused, show why: "Built-in themes can be hidden,
not removed", "Apply another theme before removing the active one".

### U7: Hover states
Show a hover state on cards, buttons and chips, consistent with NIRUDISPLAY.

Out of scope: a theme editor, network downloads, and changes to the theme TOML format or
the `niructl` JSON schema.

## 4. Gates

Automated: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`,
`cargo test --locked`, a release build, `sh tests/cli-smoke.sh` and the new MSRV job.
New tests:
- the T2 keymap fixture,
- T3 scroll clamping and wheel independence (pure state test),
- T6 LRU never evicting a visible thumbnail,
- the U2 filter and chips,
- the U3 undo target,
- U4 path validation and TOML preview parsing,
- the T9 compact layout without collisions at 520x420.

Add an Xvfb x11 test, ignored by default and run in CI. Point `NIRUTHEMESELECTOR_NIRUCTL`
at a fixture that serves a catalog of 30 themes and records argv. The test checks
wheel-scrolling to the last row, typing a path with `setxkbmap se`, apply through
double-click (the fixture sees `theme <id>`), and that a slow fixture (5 s) does not block
repainting or Esc.

Native, on **niruxwork**: the owner's work laptop, where the owner allows local testing.
- Use a separate Xvfb with `setxkbmap se` for input tests whenever possible, so the
  owner's windows are not disturbed.
- On the real session, run an idle `strace -f -e execve` for 60 s (0 spawns expected), an
  external `niructl theme` change, and an apply plus Undo. Afterwards, restore the theme
  that was active at the start (record it from `niruthemeselector status` first).
- Import a temporary test theme and remove it again. Leave no test files in the user theme
  directory.

Optionally, run a short smoke test on othala, algiz or void (owner-authorized hosts).

## 5. Release

Bump to **0.2.0** and update the README: keys, search, import, undo and the event model.
Tag `v0.2.0` through the normal workflow and verify the assets and checksums. Then pin it
in `nirucon/nirux` `manifest`:
- If `nirux/tools/suite-pin` exists, use it.
- Otherwise bump the manifest and suite version the usual way. Change only the
  niruthemeselector line, and rebase if other pin commits land at the same time.

Install through `nirux update` and verify that the running binary comes from the new
generation. Do not move or replace published tags or assets.

## 6. Final report

For each item T1–T9 and U1–U7: the change, how it was tested, and the evidence (trace
counts, test names, peak RSS before and after, native observations). Include release URLs,
the installed and running version, and remaining limitations. Mark anything unfinished as
**PARTIAL** and give the exact remaining steps.
