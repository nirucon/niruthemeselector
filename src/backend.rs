//! The canonical theme catalogue and management commands owned by niructl.

use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    os::unix::io::AsRawFd,
    path::PathBuf,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

/// `niructl theme` composes wallpapers with ffmpeg and reloads the bar and
/// the WM; `theme-import` copies and validates a picture.
pub const SLOW_DEADLINE: Duration = Duration::from_secs(30);
/// Everything else answers from small files.
pub const QUICK_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Colors {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub background: String,
    #[serde(default)]
    pub foreground: String,
    #[serde(default)]
    pub muted: String,
    #[serde(default)]
    pub border: String,
    #[serde(default)]
    pub focus: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ThemeEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub builtin: bool,
    #[serde(default)]
    pub overridden: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub valid: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub colors: Option<Colors>,
    #[serde(default)]
    pub wallpaper: Option<String>,
    #[serde(default)]
    pub wallpaper_available: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub schema: u32,
    #[serde(default)]
    pub active: String,
    #[serde(default)]
    pub themes: Vec<ThemeEntry>,
    #[serde(default)]
    pub hidden: Vec<String>,
    #[serde(default)]
    pub appearance_overrides: bool,
    #[serde(default)]
    pub follow_theme: bool,
    #[serde(default)]
    pub theme_dir: String,
    /// `art` or `signature` (newer NIRUWM). Missing, null or
    /// unknown reads as `art`, which is what an older niructl shows.
    #[serde(default)]
    pub theme_style: Option<String>,
}

impl Catalog {
    pub fn style(&self) -> WallpaperStyle {
        WallpaperStyle::parse(self.theme_style.as_deref().unwrap_or(""))
    }
}

/// Which picture each theme shows: its own artwork, or the NIRUX Signature
/// photo tinted in the theme's colours (falling back to the artwork).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WallpaperStyle {
    #[default]
    Art,
    Signature,
}

impl WallpaperStyle {
    pub fn parse(value: &str) -> Self {
        if value.trim().eq_ignore_ascii_case("signature") {
            WallpaperStyle::Signature
        } else {
            WallpaperStyle::Art
        }
    }

    /// The value niructl uses.
    pub fn id(self) -> &'static str {
        match self {
            WallpaperStyle::Art => "art",
            WallpaperStyle::Signature => "signature",
        }
    }

    /// The word the window shows.
    pub fn label(self) -> &'static str {
        match self {
            WallpaperStyle::Art => "Art",
            WallpaperStyle::Signature => "Signature",
        }
    }
}

pub fn find_niructl() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NIRUTHEMESELECTOR_NIRUCTL").map(PathBuf::from) {
        if path.is_file() {
            return Some(path);
        }
    }
    let mut candidates = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".local/share/nirux/current/bin/niructl"));
        candidates.push(home.join(".local/bin/niructl"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            candidates.push(dir.join("niructl"));
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn niructl() -> Result<PathBuf, String> {
    find_niructl().ok_or_else(|| {
        "the niructl command was not found; install NIRUX or run this from a NIRUX session"
            .to_string()
    })
}

fn run(args: &[&str]) -> Result<Output, String> {
    run_within(args, QUICK_DEADLINE)
}

/// Run `program` and collect its output, but never for longer than
/// `deadline`. The pipes are read without blocking, so a program that stops
/// answering (or leaves a child holding its stdout) cannot hang the caller;
/// it is killed and reaped when the time is up. The same pattern as
/// NIRUBAR's `probe::run` and NIRU Wallpaper's `output_within`.
pub fn output_within(program: &mut Command, deadline: Duration) -> std::io::Result<Output> {
    let started = Instant::now();
    let mut child = program
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut pipes = [
        child.stdout.take().map(Pipe::new),
        child.stderr.take().map(Pipe::new),
    ];
    let mut expired = false;
    while pipes.iter().any(|p| p.as_ref().is_some_and(|p| p.open)) {
        let Some(left) = deadline.checked_sub(started.elapsed()) else {
            expired = true;
            break;
        };
        let mut fds: Vec<libc::pollfd> = pipes
            .iter()
            .flatten()
            .filter(|p| p.open)
            .map(|p| libc::pollfd {
                fd: p.fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let ms = left.as_millis().clamp(1, i32::MAX as u128) as i32;
        // SAFETY: a live array of pollfd structures and its length.
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        for pipe in pipes.iter_mut().flatten() {
            pipe.read_available();
        }
    }
    // The pipes are closed: wait for the exit, still within the deadline.
    let status = loop {
        if expired {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("no answer within {} s", deadline.as_secs()),
            ));
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= deadline {
            expired = true;
            continue;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let [stdout, stderr] = pipes.map(|p| p.map(|p| p.data).unwrap_or_default());
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// One output pipe of a child, read without blocking.
struct Pipe {
    reader: Box<dyn Read>,
    fd: libc::c_int,
    data: Vec<u8>,
    open: bool,
}

impl Pipe {
    fn new(reader: impl Read + AsRawFd + 'static) -> Self {
        let fd = reader.as_raw_fd();
        // SAFETY: changing the flags of a descriptor this process owns.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        Self {
            reader: Box::new(reader),
            fd,
            data: Vec::new(),
            open: true,
        }
    }

    fn read_available(&mut self) {
        let mut buf = [0u8; 8192];
        while self.open {
            match self.reader.read(&mut buf) {
                Ok(0) => self.open = false,
                Ok(n) => self.data.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => self.open = false,
            }
        }
    }
}

fn run_within(args: &[&str], deadline: Duration) -> Result<Output, String> {
    let bin = niructl()?;
    let out = output_within(Command::new(&bin).args(args), deadline).map_err(|error| {
        if error.kind() == std::io::ErrorKind::TimedOut {
            format!(
                "niructl gave no answer within {} s and was stopped",
                deadline.as_secs()
            )
        } else {
            format!("cannot run {}: {error}", bin.display())
        }
    })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let why = err
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .unwrap_or("no explanation");
        return Err(why.to_string());
    }
    Ok(out)
}

pub fn catalog() -> Result<Catalog, String> {
    let out = run(&["theme-catalog"])?;
    serde_json::from_slice(&out.stdout)
        .map_err(|error| format!("invalid theme catalogue JSON: {error}"))
}

pub fn apply(id: &str) -> Result<(), String> {
    run_within(&["theme", id], SLOW_DEADLINE).map(|_| ())
}

/// `niructl wallpaper style toggle`: it writes the setting and waits for
/// the running WM to reload, like `niructl theme`.
pub fn toggle_wallpaper_style() -> Result<(), String> {
    run_within(&["wallpaper", "style", "toggle"], SLOW_DEADLINE)
        .map(|_| ())
        .map_err(|error| style_error(&error))
}

/// A short reason for a failed style switch. A niructl older than the
/// setting answers with its whole usage text.
fn style_error(error: &str) -> String {
    if error.contains("Usage:") {
        "This niructl cannot switch the wallpaper style yet; update NIRUX.".into()
    } else {
        format!("Cannot switch the wallpaper style: {error}")
    }
}

pub fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        return home.join(rest);
    }
    PathBuf::from(path)
}

pub fn import(theme: &str, wallpaper: &str, id: &str, replace: bool) -> Result<(), String> {
    let mut args: Vec<String> = vec![
        "theme-import".into(),
        expand_home(theme).display().to_string(),
    ];
    if !id.trim().is_empty() {
        args.push("--id".into());
        args.push(id.trim().into());
    }
    if !wallpaper.trim().is_empty() {
        args.push("--wallpaper".into());
        args.push(expand_home(wallpaper).display().to_string());
    }
    if replace {
        args.push("--replace".into());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_within(&refs, SLOW_DEADLINE).map(|_| ())
}

pub fn remove(id: &str) -> Result<(), String> {
    run(&["theme-remove", id]).map(|_| ())
}

pub fn hide(id: &str, hide: bool) -> Result<(), String> {
    run(&[if hide { "theme-hide" } else { "theme-show" }, id]).map(|_| ())
}

/// One management action, run on the action worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Apply {
        id: String,
        name: String,
    },
    /// Undo: apply the theme that was active before the last Apply.
    Restore {
        id: String,
        name: String,
    },
    Import {
        theme: String,
        wallpaper: String,
        id: String,
        replace: bool,
    },
    Remove {
        id: String,
        name: String,
    },
    Hide {
        id: String,
        name: String,
        hide: bool,
    },
    /// Switch every theme's picture between Art and Signature.
    WallpaperStyle,
}

impl Action {
    /// The busy text the window shows while this runs.
    pub fn busy_text(&self) -> String {
        match self {
            Action::Apply { name, .. } => format!("Applying {name}\u{2026}"),
            Action::Restore { name, .. } => format!("Restoring {name}\u{2026}"),
            Action::Import { .. } => "Importing the theme\u{2026}".into(),
            Action::Remove { name, .. } => format!("Removing {name}\u{2026}"),
            Action::Hide { name, hide, .. } => {
                format!(
                    "{} {name}\u{2026}",
                    if *hide { "Hiding" } else { "Showing" }
                )
            }
            Action::WallpaperStyle => "Switching the wallpaper style\u{2026}".into(),
        }
    }

    /// Run the action, then read the catalogue again, both within their
    /// deadlines. Called on the worker thread only.
    pub fn perform(&self) -> (Result<(), String>, Option<Result<Catalog, String>>) {
        let result = match self {
            Action::Apply { id, .. } | Action::Restore { id, .. } => apply(id),
            Action::Import {
                theme,
                wallpaper,
                id,
                replace,
            } => import(theme, wallpaper, id, *replace),
            Action::Remove { id, .. } => remove(id),
            Action::Hide { id, hide, .. } => self::hide(id, *hide),
            Action::WallpaperStyle => toggle_wallpaper_style(),
        };
        (result, Some(catalog()))
    }
}

pub fn status_text() -> Result<String, String> {
    let catalog = catalog()?;
    let mut text = format!(
        "active={}\nappearance_overrides={}\nfollow_theme={}\ntheme_dir={}\ntheme_style={}\n",
        catalog.active,
        catalog.appearance_overrides,
        catalog.follow_theme,
        catalog.theme_dir,
        catalog.style().id()
    );
    for entry in catalog.themes {
        let origin = if entry.overridden {
            "built-in-override"
        } else if entry.builtin {
            "built-in"
        } else {
            "user"
        };
        text.push_str(&format!(
            "theme={}\tname={}\torigin={}\thidden={}\tvalid={}\twallpaper={}\n",
            entry.id, entry.name, origin, entry.hidden, entry.valid, entry.wallpaper_available
        ));
    }
    for id in catalog.hidden {
        text.push_str(&format!("hidden={id}\n"));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "schema": 1,
      "active": "niru-nord",
      "themes": [
        {"id":"niru-nord","name":"Niru Nord","builtin":true,"overridden":false,"hidden":false,"valid":true,"error":null,"colors":{"name":"Niru Nord","background":"2e3440","foreground":"eceff4","muted":"9aa5b1","border":"3b4252","focus":"88c0d0"},"wallpaper":"/p/niru-nord.png","wallpaper_available":true},
        {"id":"broken","name":"Broken","builtin":false,"overridden":false,"hidden":false,"valid":false,"error":"bad colour","colors":null,"wallpaper":null,"wallpaper_available":false}
      ],
      "hidden": [], "appearance_overrides": false, "follow_theme": true,
      "theme_dir": "~/Pictures/Wallpapers/NIRU"
    }"#;

    #[test]
    fn parses_the_catalogue() {
        let catalog: Catalog = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(catalog.active, "niru-nord");
        assert_eq!(catalog.themes.len(), 2);
        assert_eq!(catalog.themes[0].colors.as_ref().unwrap().focus, "88c0d0");
        assert!(!catalog.themes[1].valid);
        assert_eq!(catalog.themes[1].error.as_deref(), Some("bad colour"));
        // An older niructl has no theme_style: that is Art.
        assert_eq!(catalog.theme_style, None);
        assert_eq!(catalog.style(), WallpaperStyle::Art);
    }

    #[test]
    fn reads_the_wallpaper_style() {
        let with = |value: &str| {
            let json = SAMPLE.replacen(
                "\"schema\": 1,",
                &format!("\"schema\": 1, \"theme_style\": {value},"),
                1,
            );
            serde_json::from_str::<Catalog>(&json).unwrap()
        };
        let catalog = with("\"signature\"");
        assert_eq!(catalog.style(), WallpaperStyle::Signature);
        assert_eq!(catalog.themes.len(), 2, "the rest still parses");
        assert_eq!(with("\"art\"").style(), WallpaperStyle::Art);
        assert_eq!(with("null").style(), WallpaperStyle::Art);
        assert_eq!(with("\"future\"").style(), WallpaperStyle::Art);
        assert_eq!(WallpaperStyle::Signature.label(), "Signature");
        assert_eq!(WallpaperStyle::Art.id(), "art");
        assert_eq!(Catalog::default().style(), WallpaperStyle::Art);
    }

    #[test]
    fn an_old_niructl_gets_a_short_style_error() {
        let old = style_error("niructl: Usage: niructl status | tag N | movetotag N | reset");
        assert!(old.contains("update NIRUX"), "{old}");
        assert!(old.len() < 120);
        assert_eq!(
            style_error("no answer"),
            "Cannot switch the wallpaper style: no answer"
        );
        assert_eq!(
            Action::WallpaperStyle.busy_text(),
            "Switching the wallpaper style\u{2026}"
        );
    }

    #[test]
    fn a_program_that_does_not_answer_is_stopped_at_the_deadline() {
        let started = Instant::now();
        let error = output_within(
            Command::new("sh").args(["-c", "sleep 30"]),
            Duration::from_millis(300),
        )
        .expect_err("the deadline must stop it");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(3));
        // A child that keeps stdout open after its parent exits cannot hang
        // the caller either.
        let started = Instant::now();
        let _ = output_within(
            Command::new("sh").args(["-c", "sleep 30 & echo started"]),
            Duration::from_millis(300),
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        let out = output_within(
            Command::new("sh").args(["-c", "echo out; echo err >&2"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
    }

    #[test]
    fn expands_home_paths() {
        std::env::set_var("HOME", "/tmp/theme-home");
        assert_eq!(expand_home("~/x"), PathBuf::from("/tmp/theme-home/x"));
        assert_eq!(expand_home("/absolute"), PathBuf::from("/absolute"));
    }
}
