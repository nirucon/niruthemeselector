//! Knowing when the desktop's answer may have changed, without asking.
//!
//! The window used to run `niructl theme-catalog` every 5 s to find out. Now
//! it watches, with inotify, the files `niructl` changes for `theme`,
//! `theme-import`, `theme-remove` and `theme-hide`/`theme-show`: the NIRUWM
//! config (`theme = …`), the user theme directory, the hidden list
//! (`theme-preferences.toml`) and the theme wallpaper directory. A burst of
//! changes is debounced into one refresh. Shared with NIRU Wallpaper 0.2.0.
//! Worker threads wake the event loop through a pipe, so the loop can block
//! in `poll` with no timeout when nothing is pending.

use std::{
    ffi::CString,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// How long the window waits after the last change before it refreshes.
pub const DEBOUNCE: Duration = Duration::from_millis(300);
/// A steady stream of changes still refreshes at least this often.
pub const DEBOUNCE_MAX: Duration = Duration::from_millis(1000);

/// A trailing debounce: the refresh runs `DEBOUNCE` after the last change,
/// and never later than `DEBOUNCE_MAX` after the first one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Debounce {
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Debounce {
    /// Record one change.
    pub fn note(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    fn due_at(&self) -> Option<Instant> {
        let first = self.first?;
        let last = self.last?;
        Some((last + DEBOUNCE).min(first + DEBOUNCE_MAX))
    }

    /// How long the event loop may sleep, or `None` for "until an event".
    pub fn timeout(&self, now: Instant) -> Option<Duration> {
        self.due_at().map(|at| at.saturating_duration_since(now))
    }

    /// True once, when the refresh is due; the debounce is then reset.
    pub fn take(&mut self, now: Instant) -> bool {
        match self.due_at() {
            Some(at) if now >= at => {
                *self = Self::default();
                true
            }
            _ => false,
        }
    }

    #[cfg(test)]
    pub fn pending(&self) -> bool {
        self.first.is_some()
    }
}

/// A pipe a worker thread writes one byte to, to wake the event loop.
pub struct Wake {
    read: libc::c_int,
    write: libc::c_int,
}

impl Wake {
    pub fn new() -> Result<Self, String> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` has room for the two descriptors pipe2 returns.
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if rc != 0 {
            return Err(format!(
                "cannot create a wake-up pipe: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self {
            read: fds[0],
            write: fds[1],
        })
    }

    pub fn fd(&self) -> libc::c_int {
        self.read
    }

    /// Wake the event loop. A full pipe already means "wake up".
    pub fn notify(&self) {
        let byte = 1u8;
        // SAFETY: one byte from a live buffer to a descriptor this owns.
        let _ = unsafe { libc::write(self.write, (&byte as *const u8).cast(), 1) };
    }

    /// Empty the pipe after waking.
    pub fn drain(&self) {
        let mut buf = [0u8; 64];
        loop {
            // SAFETY: reads into a live buffer of the given length.
            let n = unsafe { libc::read(self.read, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                return;
            }
        }
    }
}

impl Drop for Wake {
    fn drop(&mut self) {
        // SAFETY: both descriptors are owned here and closed once.
        unsafe {
            libc::close(self.read);
            libc::close(self.write);
        }
    }
}

/// What one watched directory is for, which decides which names matter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `~/.config/niruwm`: only the files `niructl theme*` and the palette read.
    Config,
    /// The user theme directory or the theme wallpaper directory: any
    /// visible entry.
    Files,
    /// A missing directory's nearest parent: any entry, so its creation
    /// is noticed and the watch moves to it.
    Parent,
}

/// True when an event on `name` in a directory of this kind can change what
/// the window shows.
pub fn relevant(kind: Kind, name: &str) -> bool {
    match kind {
        // config.toml holds `theme = …` (what `niructl theme` writes),
        // theme-preferences.toml the hidden list, appearance.toml the colour
        // overrides, and themes/ the user theme files.
        Kind::Config => matches!(
            name,
            "config.toml" | "appearance.toml" | "theme-preferences.toml" | "themes" | ""
        ),
        // Hidden names are temporary files (niructl's transaction copies, an
        // editor's swap file); their final rename is what counts.
        Kind::Files => name.is_empty() || !name.starts_with('.'),
        Kind::Parent => true,
    }
}

/// The directories whose contents decide `niructl theme-catalog`'s answer:
/// the NIRUWM config directory, its `themes/` directory and the theme
/// wallpaper directory (`theme_dir`, `~/` allowed).
pub fn watched_dirs(theme_dir: &str) -> Vec<(PathBuf, Kind)> {
    let config = crate::theme::config_dir();
    let mut out = vec![
        (config.clone(), Kind::Config),
        (config.join("themes"), Kind::Files),
    ];
    if !theme_dir.trim().is_empty() {
        let path = crate::backend::expand_home(theme_dir);
        if !out.iter().any(|(p, _)| *p == path) {
            out.push((path, Kind::Files));
        }
    }
    out
}

const MASK: u32 = libc::IN_CLOSE_WRITE
    | libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR;

/// An inotify instance watching a set of directories.
pub struct Watcher {
    fd: libc::c_int,
    /// Watch descriptor, the directory as asked for, and its kind.
    watches: Vec<(libc::c_int, PathBuf, Kind)>,
    /// The set last asked for, so an unchanged set is not re-added.
    wanted: Vec<(PathBuf, Kind)>,
}

impl Watcher {
    pub fn new() -> Result<Self, String> {
        // SAFETY: plain syscall with flags.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(format!(
                "cannot watch the theme files: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self {
            fd,
            watches: Vec::new(),
            wanted: Vec::new(),
        })
    }

    pub fn fd(&self) -> libc::c_int {
        self.fd
    }

    fn add(&mut self, path: &Path, kind: Kind) -> bool {
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: a valid NUL-terminated path and a live inotify descriptor.
        let wd = unsafe { libc::inotify_add_watch(self.fd, c.as_ptr(), MASK) };
        if wd < 0 {
            return false;
        }
        if !self.watches.iter().any(|(w, _, _)| *w == wd) {
            self.watches.push((wd, path.to_path_buf(), kind));
        }
        true
    }

    /// Watch exactly these directories. A directory that does not exist yet
    /// is watched through its nearest existing parent.
    pub fn set(&mut self, dirs: Vec<(PathBuf, Kind)>) {
        let all_present = self.watches.iter().all(|(_, _, k)| *k != Kind::Parent);
        if dirs == self.wanted && all_present {
            return;
        }
        for (wd, _, _) in self.watches.drain(..) {
            // SAFETY: removing a watch this instance added.
            unsafe { libc::inotify_rm_watch(self.fd, wd) };
        }
        for (path, kind) in &dirs {
            if self.add(path, *kind) {
                continue;
            }
            let mut parent = path.parent();
            while let Some(dir) = parent {
                if dir.is_dir() {
                    self.add(dir, Kind::Parent);
                    break;
                }
                parent = dir.parent();
            }
        }
        self.wanted = dirs;
    }

    /// Read every queued event. Returns true when one of them can change what
    /// the window shows.
    pub fn drain(&mut self) -> bool {
        let mut relevant_seen = false;
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: reads into a live, suitably sized buffer.
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                return relevant_seen;
            }
            let n = n as usize;
            let mut at = 0;
            let header = std::mem::size_of::<libc::inotify_event>();
            while at + header <= n {
                // SAFETY: the kernel wrote a whole event header at `at`;
                // read_unaligned copes with the byte buffer's alignment.
                let event: libc::inotify_event =
                    unsafe { std::ptr::read_unaligned(buf.as_ptr().add(at).cast()) };
                let len = event.len as usize;
                let raw = &buf[(at + header).min(n)..(at + header + len).min(n)];
                let name = raw.split(|b| *b == 0).next().unwrap_or_default();
                let name = String::from_utf8_lossy(name);
                let kind = self
                    .watches
                    .iter()
                    .find(|(wd, _, _)| *wd == event.wd)
                    .map(|(_, _, kind)| *kind);
                if event.mask & libc::IN_Q_OVERFLOW != 0 {
                    relevant_seen = true;
                }
                if let Some(kind) = kind {
                    if event.mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_IGNORED)
                        != 0
                    {
                        // The directory itself went away: watch its parent
                        // on the next `set`.
                        self.wanted.clear();
                        relevant_seen = true;
                    } else if relevant(kind, &name) {
                        relevant_seen = true;
                    }
                }
                at += header + len;
            }
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // SAFETY: the descriptor is owned here and closed once.
        unsafe { libc::close(self.fd) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_of_changes_is_one_refresh_after_the_last() {
        let t0 = Instant::now();
        let mut d = Debounce::default();
        assert_eq!(
            d.timeout(t0),
            None,
            "nothing pending: block without a timeout"
        );
        assert!(!d.take(t0));
        d.note(t0);
        d.note(t0 + Duration::from_millis(100));
        d.note(t0 + Duration::from_millis(200));
        assert!(!d.take(t0 + Duration::from_millis(400)), "too early");
        assert_eq!(
            d.timeout(t0 + Duration::from_millis(400)),
            Some(Duration::from_millis(100))
        );
        assert!(
            d.take(t0 + Duration::from_millis(500)),
            "300 ms after the last"
        );
        assert!(!d.take(t0 + Duration::from_millis(600)), "only once");
        assert!(!d.pending());
        assert_eq!(d.timeout(t0 + Duration::from_millis(600)), None);
    }

    #[test]
    fn a_steady_stream_still_refreshes_within_a_second() {
        let t0 = Instant::now();
        let mut d = Debounce::default();
        let mut refreshed_at = None;
        for step in 0..30u64 {
            let now = t0 + Duration::from_millis(step * 100);
            if d.take(now) {
                refreshed_at = Some(step * 100);
                break;
            }
            d.note(now);
        }
        assert_eq!(refreshed_at, Some(1000));
    }

    #[test]
    fn only_the_files_niructl_reads_count() {
        assert!(relevant(Kind::Config, "config.toml"));
        assert!(relevant(Kind::Config, "appearance.toml"));
        assert!(relevant(Kind::Config, "theme-preferences.toml"));
        assert!(relevant(Kind::Config, "themes"));
        assert!(!relevant(Kind::Config, "displays.toml"));
        assert!(!relevant(Kind::Config, "config.toml.bak"));
        assert!(relevant(Kind::Files, "mine.toml"));
        assert!(!relevant(Kind::Files, ".mine.toml.tmp"));
        assert!(relevant(Kind::Parent, "Wallpapers"));
    }

    #[test]
    fn a_new_picture_and_a_new_directory_are_noticed() {
        let root = std::env::temp_dir().join(format!("nts-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("existing")).unwrap();
        let mut w = Watcher::new().unwrap();
        w.set(vec![
            (root.join("existing"), Kind::Files),
            (root.join("later/NIRU"), Kind::Files),
        ]);
        assert!(!w.drain(), "nothing happened yet");
        std::fs::write(root.join("existing/.temp"), b"x").unwrap();
        assert!(!w.drain(), "a hidden temporary file is not a change");
        std::fs::rename(root.join("existing/.temp"), root.join("existing/a.png")).unwrap();
        assert!(w.drain(), "the rename into place is");
        // The missing directory is watched through its parent.
        std::fs::create_dir_all(root.join("later/NIRU")).unwrap();
        assert!(w.drain());
        w.set(vec![
            (root.join("existing"), Kind::Files),
            (root.join("later/NIRU"), Kind::Files),
        ]);
        std::fs::write(root.join("later/NIRU/b.png"), b"x").unwrap();
        assert!(w.drain(), "the directory is watched itself once it exists");
        std::fs::remove_dir_all(root).unwrap();
    }
}
