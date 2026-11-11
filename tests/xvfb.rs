//! The window under a real X server, against a fixture `niructl`.
//!
//! Ignored by default because it needs `Xvfb`, `xdotool`, `xwd` and
//! `setxkbmap`; CI runs it with `cargo test --locked --test xvfb --
//! --ignored`. The fixture serves a catalogue of 30 themes, records every
//! argument vector it is started with, and takes 5 seconds for `theme` when
//! told to. `wallpaper style toggle` flips the catalogue's `theme_style`.

use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

const FIXTURE: &str = r#"#!/bin/sh
FX=${NTS_FX:?}
printf '%s\n' "$*" >> "$FX/argv.log"
active=$(cat "$FX/active" 2>/dev/null || echo t01)
style=$(cat "$FX/style" 2>/dev/null || echo art)
case "$1" in
theme-catalog)
  printf '{"schema":1,"active":"%s","themes":[' "$active"
  i=1; sep=
  while [ $i -le 30 ]; do
    id=$(printf 't%02d' $i)
    printf '%s{"id":"%s","name":"Theme %02d","builtin":true,"overridden":false,"hidden":false,"valid":true,"error":null,"colors":{"name":"Theme %02d","background":"%02x2030","foreground":"eeeeee","muted":"999999","border":"333333","focus":"88c0d0"},"wallpaper":"%s/walls/%s.png","wallpaper_available":true}' "$sep" "$id" $i $i $((i*8)) "$FX" "$id"
    sep=,; i=$((i+1))
  done
  printf '],"hidden":[],"appearance_overrides":false,"follow_theme":true,"theme_dir":"%s/walls","theme_style":"%s"}\n' "$FX" "$style" ;;
wallpaper)
  [ "$2 $3" = "style toggle" ] || { echo "fixture: $*" >&2; exit 2; }
  if [ "$style" = art ]; then style=signature; else style=art; fi
  printf '%s' "$style" > "$FX/style"
  echo "theme_style=$style" ;;
theme)
  if [ -f "$FX/slow" ]; then sleep "$(cat "$FX/slow")"; fi
  printf '%s' "$2" > "$FX/active"
  printf 'done %s\n' "$2" >> "$FX/argv.log" ;;
theme-import|theme-remove|theme-hide|theme-show) echo ok ;;
*) echo "fixture: $*" >&2; exit 2 ;;
esac
"#;

/// Everything the test started, stopped and removed however it ends.
struct Session {
    root: PathBuf,
    xvfb: Child,
    app: Option<Child>,
    display: String,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(app) = self.app.as_mut() {
            let _ = app.kill();
            let _ = app.wait();
        }
        let _ = self.xvfb.kill();
        let _ = self.xvfb.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Session {
    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("argv.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn run(&self, program: &str, args: &[&str]) -> String {
        let out = Command::new(program)
            .args(args)
            .env("DISPLAY", &self.display)
            .output()
            .unwrap_or_else(|e| panic!("{program}: {e}"));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn xdotool(&self, args: &[&str]) -> String {
        self.run("xdotool", args)
    }

    fn screenshot(&self, window: &str) -> Vec<u8> {
        Command::new("xwd")
            .args(["-silent", "-id", window])
            .env("DISPLAY", &self.display)
            .output()
            .expect("xwd")
            .stdout
    }

    /// Wait until the fixture log satisfies `test`, or fail after `limit`.
    fn wait_log(&self, limit: Duration, what: &str, test: impl Fn(&[String]) -> bool) {
        let start = Instant::now();
        while !test(&self.log()) {
            assert!(
                start.elapsed() < limit,
                "timed out waiting for {what}: {:?}",
                self.log()
            );
            sleep(Duration::from_millis(25));
        }
    }

    fn exited(&mut self) -> bool {
        self.app
            .as_mut()
            .is_some_and(|app| app.try_wait().ok().flatten().is_some())
    }
}

fn start() -> Session {
    let root = std::env::temp_dir().join(format!("nts-xvfb-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("walls")).unwrap();
    fs::create_dir_all(root.join("home/.config/niruwm")).unwrap();
    fs::create_dir_all(root.join("home/Teman")).unwrap();
    fs::create_dir_all(root.join("cache")).unwrap();
    for i in 1..=30u8 {
        image::RgbImage::from_pixel(64, 36, image::Rgb([i * 8, 255 - i * 8, 120]))
            .save(root.join(format!("walls/t{i:02}.png")))
            .unwrap();
    }
    fs::write(
        root.join("home/.config/niruwm/config.toml"),
        "theme = \"t01\"\n",
    )
    .unwrap();
    fs::write(
        root.join("home/Teman/$x@Å.toml"),
        "name = \"Typed\"\nbackground = \"000000\"\nforeground = \"ffffff\"\nmuted = \"888888\"\nborder = \"222222\"\nfocus = \"ff0000\"\n",
    )
    .unwrap();
    let niructl = root.join("niructl");
    fs::write(&niructl, FIXTURE).unwrap();
    fs::set_permissions(&niructl, fs::Permissions::from_mode(0o755)).unwrap();
    // Xvfb picks a free display and names it on the descriptor given, so
    // the test never collides with another X server on the host.
    let mut xvfb = Command::new("Xvfb")
        .args([
            "-displayfd",
            "1",
            "-screen",
            "0",
            "1280x800x24",
            "-nolisten",
            "tcp",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("Xvfb must be installed for this test");
    let mut line = String::new();
    BufReader::new(xvfb.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let display = format!(":{}", line.trim());
    Session {
        root,
        xvfb,
        app: None,
        display,
    }
}

fn open_window(s: &mut Session) -> String {
    let app = Command::new(env!("CARGO_BIN_EXE_niruthemeselector"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("DISPLAY", &s.display)
        .env("HOME", s.home())
        .env("XDG_CACHE_HOME", s.root.join("cache"))
        .env("NIRUTHEMESELECTOR_NIRUCTL", s.root.join("niructl"))
        .env("NTS_FX", &s.root)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    s.app = Some(app);
    let start = Instant::now();
    loop {
        let id = s.xdotool(&["search", "--name", "NIRU Theme Selector"]);
        if let Some(id) = id.lines().next().filter(|l| !l.is_empty()) {
            return id.to_string();
        }
        assert!(start.elapsed() < Duration::from_secs(20), "no window");
        sleep(Duration::from_millis(100));
    }
}

fn count(log: &[String], line: &str) -> usize {
    log.iter().filter(|l| *l == line).count()
}

#[test]
#[ignore = "needs Xvfb, xdotool, xwd and setxkbmap; CI runs it with --ignored"]
fn wheel_typing_double_click_undo_and_a_slow_apply_under_xvfb() {
    let mut s = start();
    let window = open_window(&mut s);
    s.wait_log(Duration::from_secs(10), "the first catalogue read", |l| {
        l.iter().any(|x| x == "theme-catalog")
    });
    sleep(Duration::from_secs(2));

    // T5: an idle window starts no process.
    let before = s.log().len();
    sleep(Duration::from_secs(10));
    assert_eq!(
        s.log().len(),
        before,
        "spawns while idle: {:?}",
        &s.log()[before..]
    );

    // T5: a theme change made elsewhere is read again within a second.
    let config = s.home().join(".config/niruwm/config.toml");
    fs::write(config.with_extension("tmp"), "theme = \"t02\"\n").unwrap();
    fs::rename(config.with_extension("tmp"), &config).unwrap();
    s.wait_log(Duration::from_secs(1), "the refresh after a change", |l| {
        l[before..].iter().any(|x| x == "theme-catalog")
    });
    sleep(Duration::from_millis(500));

    // T3: twelve wheel notches reach the last row (30 themes, 3 columns,
    // 3 rows in view at 1120x760). The wheel selects nothing.
    s.xdotool(&["mousemove", "--window", &window, "300", "300"]);
    for _ in 0..12 {
        s.xdotool(&["click", "5"]);
    }
    sleep(Duration::from_millis(300));
    assert!(
        !s.log().iter().any(|l| l.starts_with("theme ")),
        "the wheel applied something"
    );

    // U3: double-clicking the bottom-right card, Theme 30, applies it.
    s.xdotool(&["mousemove", "--window", &window, "640", "520"]);
    s.xdotool(&["click", "--repeat", "2", "--delay", "80", "1"]);
    s.wait_log(Duration::from_secs(5), "the apply of t30", |l| {
        count(l, "done t30") == 1
    });
    assert_eq!(count(&s.log(), "theme t30"), 1, "applied once");
    sleep(Duration::from_millis(500));

    // U3: Ctrl+Z re-applies the theme that was active before.
    s.xdotool(&["key", "ctrl+z"]);
    s.wait_log(Duration::from_secs(5), "the undo", |l| {
        count(l, "done t01") == 1
    });

    // Alt+S switches the wallpaper style through niructl, then reads the
    // catalogue again.
    let before = s.log().len();
    s.xdotool(&["key", "alt+s"]);
    s.wait_log(Duration::from_secs(5), "the style toggle", |l| {
        let new = &l[before..];
        new.iter()
            .position(|x| x == "wallpaper style toggle")
            .is_some_and(|at| new[at..].iter().any(|x| x == "theme-catalog"))
    });
    assert_eq!(
        fs::read_to_string(s.root.join("style")).unwrap(),
        "signature"
    );
    sleep(Duration::from_millis(500));

    // T2: a Swedish keymap types the documented path into the import
    // dialog. xdotool presses the level's modifiers itself: `dead_tilde`
    // is AltGr + the ¨ key (core column 5), `dollar` and `at` are AltGr+4
    // and AltGr+2, `slash` is Shift+7, so the window sees the real
    // ISO_Level3_Shift (Mod5) and Shift states.
    s.run("setxkbmap", &["se"]);
    sleep(Duration::from_millis(500));
    s.xdotool(&["key", "alt+i"]);
    sleep(Duration::from_millis(300));
    s.xdotool(&[
        "key",
        "--delay",
        "40",
        "dead_tilde",
        "slash",
        "T",
        "e",
        "m",
        "a",
        "n",
        "slash",
        "dollar",
        "x",
        "at",
        "Aring",
        "period",
        "t",
        "o",
        "m",
        "l",
    ]);
    sleep(Duration::from_millis(300));
    s.xdotool(&["key", "Return"]);
    let expected = format!("theme-import {}/Teman/$x@Å.toml", s.home().display());
    s.wait_log(Duration::from_secs(5), "the typed import", |l| {
        l.iter().any(|x| x.starts_with("theme-import"))
    });
    assert!(
        s.log().contains(&expected),
        "expected {expected:?} in {:?}",
        s.log()
    );
    sleep(Duration::from_millis(500));

    // U4: Ctrl+V pastes the CLIPBOARD (here an absolute path) into the
    // focused field through a normal selection conversion.
    let pasted = format!("{}/Teman/$x@Å.toml", s.home().display());
    let mut clip = Command::new("xclip")
        .args(["-selection", "clipboard", "-loops", "1"])
        .env("DISPLAY", &s.display)
        .stdin(Stdio::piped())
        .spawn()
        .expect("xclip must be installed for this test");
    {
        use std::io::Write;
        let mut stdin = clip.stdin.take().unwrap();
        stdin.write_all(pasted.as_bytes()).unwrap();
    }
    sleep(Duration::from_millis(300));
    s.xdotool(&["key", "alt+i"]);
    sleep(Duration::from_millis(300));
    s.xdotool(&["key", "ctrl+v"]);
    sleep(Duration::from_millis(500));
    s.xdotool(&["key", "Return"]);
    s.wait_log(Duration::from_secs(5), "the pasted import", |l| {
        l.iter().filter(|x| x.starts_with("theme-import")).count() == 2
    });
    assert_eq!(count(&s.log(), &format!("theme-import {pasted}")), 2);
    let _ = clip.kill();
    let _ = clip.wait();
    sleep(Duration::from_millis(500));

    // T4: a 5 s Apply does not block repainting or Esc.
    fs::write(s.root.join("slow"), "5").unwrap();
    s.xdotool(&["mousemove", "--window", &window, "640", "520"]);
    s.xdotool(&["click", "--repeat", "2", "--delay", "80", "1"]);
    s.wait_log(Duration::from_secs(5), "the slow apply", |l| {
        count(l, "theme t30") == 2
    });
    sleep(Duration::from_millis(500));
    let applying = s.screenshot(&window);
    s.xdotool(&["key", "F1"]);
    sleep(Duration::from_millis(500));
    let help = s.screenshot(&window);
    assert!(!applying.is_empty() && !help.is_empty());
    assert_ne!(applying, help, "the window did not repaint while applying");
    // A second double-click while applying does not apply twice.
    s.xdotool(&["key", "Escape"]);
    s.xdotool(&["click", "--repeat", "2", "--delay", "80", "1"]);
    sleep(Duration::from_millis(300));
    assert_eq!(count(&s.log(), "theme t30"), 2, "applied twice while busy");
    // Esc still closes the window at once, before the Apply finished.
    let pressed = Instant::now();
    s.xdotool(&["key", "Escape"]);
    while !s.exited() {
        assert!(
            pressed.elapsed() < Duration::from_secs(2),
            "Esc did not close the window during a slow Apply"
        );
        sleep(Duration::from_millis(25));
    }
    assert_eq!(
        count(&s.log(), "done t30"),
        1,
        "the slow Apply finished too early"
    );
    // The fixture's niructl finishes its work on its own.
    s.wait_log(Duration::from_secs(10), "the orphaned apply", |l| {
        count(l, "done t30") == 2
    });
}
