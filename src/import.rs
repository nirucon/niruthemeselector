//! The import form's checks: does each path exist, what does the theme file
//! say, and the small directory list behind **Browse…**.
//!
//! Everything here only reads. The theme file is parsed as data with `toml`
//! exactly like `niructl theme-import` does (six-digit colours, no unknown
//! keys), so a file that would fail later fails here first, with the reason.

use serde::Deserialize;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

/// `niructl theme-import --wallpaper` validates the picture with the `png`
/// crate (`validate_theme_png`): only PNG, at most 256 MiB and 80 MP.
pub const WALLPAPER_LIMIT: u64 = 256 * 1024 * 1024;
/// A theme file is five colours and a name; anything larger is not one.
const THEME_LIMIT: u64 = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathStatus {
    Empty,
    Found,
    NotFound,
    /// It exists, but cannot be imported (a directory, not a PNG, too large).
    Unusable(String),
}

impl PathStatus {
    pub fn label(&self) -> String {
        match self {
            PathStatus::Empty => String::new(),
            PathStatus::Found => "\u{2713} found".into(),
            PathStatus::NotFound => "\u{2717} not found".into(),
            PathStatus::Unusable(why) => format!("\u{2717} {why}"),
        }
    }

    pub fn ok(&self) -> bool {
        matches!(self, PathStatus::Found)
    }
}

/// What a theme file would import as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    pub name: String,
    /// background, foreground, muted, border, focus
    pub colors: [u32; 5],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    name: String,
    background: String,
    foreground: String,
    muted: String,
    border: String,
    focus: String,
}

/// Parse a theme file's text the way `niructl theme-import` does.
pub fn parse_theme(text: &str) -> Result<Preview, String> {
    let theme: ThemeFile = toml::from_str(text).map_err(|error| {
        let message = error.message().trim().to_string();
        format!("not a NIRUX theme: {message}")
    })?;
    let mut colors = [0u32; 5];
    for (slot, (key, value)) in colors.iter_mut().zip([
        ("background", &theme.background),
        ("foreground", &theme.foreground),
        ("muted", &theme.muted),
        ("border", &theme.border),
        ("focus", &theme.focus),
    ]) {
        if value.len() != 6 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{key} must be six hex digits, not \"{value}\""));
        }
        *slot = u32::from_str_radix(value, 16).map_err(|error| error.to_string())?;
    }
    Ok(Preview {
        name: theme.name,
        colors,
    })
}

/// The status of the theme path and, when it is readable, its preview.
pub fn check_theme(path: &str) -> (PathStatus, Option<Result<Preview, String>>) {
    if path.trim().is_empty() {
        return (PathStatus::Empty, None);
    }
    let path = crate::backend::expand_home(path.trim());
    let Ok(meta) = fs::metadata(&path) else {
        return (PathStatus::NotFound, None);
    };
    if !meta.is_file() {
        return (PathStatus::Unusable("not a file".into()), None);
    }
    if meta.len() > THEME_LIMIT {
        return (PathStatus::Unusable("too large for a theme".into()), None);
    }
    let preview = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read it: {error}"))
        .and_then(|text| parse_theme(&text));
    (PathStatus::Found, Some(preview))
}

/// The status of the optional wallpaper path. Only PNG is accepted, so a
/// JPEG is refused here, with that reason, instead of failing in niructl.
pub fn check_wallpaper(path: &str) -> PathStatus {
    if path.trim().is_empty() {
        return PathStatus::Empty;
    }
    let path = crate::backend::expand_home(path.trim());
    let Ok(meta) = fs::metadata(&path) else {
        return PathStatus::NotFound;
    };
    if !meta.is_file() {
        return PathStatus::Unusable("not a file".into());
    }
    if meta.len() > WALLPAPER_LIMIT {
        return PathStatus::Unusable("larger than 256 MiB".into());
    }
    let mut magic = [0u8; 8];
    let is_png = fs::File::open(&path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .map(|()| magic == *b"\x89PNG\r\n\x1a\n")
        .unwrap_or(false);
    if is_png {
        PathStatus::Found
    } else {
        PathStatus::Unusable("not a PNG; niructl imports PNG wallpapers only".into())
    }
}

/// Show a path with `~/` for the home directory, as the user would type it.
pub fn display_path(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home {
        if let Ok(rest) = path.strip_prefix(home) {
            if rest.as_os_str().is_empty() {
                return "~".into();
            }
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// Where **Browse…** starts: `~/Downloads`, or `~` without one.
pub fn start_dir(home: Option<&Path>) -> PathBuf {
    let home = home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"));
    let downloads = home.join("Downloads");
    if downloads.is_dir() {
        downloads
    } else {
        home
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
}

/// Which files a browse list offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    Theme,
    Wallpaper,
}

/// The visible directories and the matching files of `dir`: `.toml` for a
/// theme, `.png` for a wallpaper. Directories first, both sorted
/// case-insensitively; hidden entries are left out.
pub fn list_dir(dir: &Path, want: Want) -> Result<Vec<Entry>, String> {
    let read = fs::read_dir(dir).map_err(|error| format!("cannot open the folder: {error}"))?;
    let mut entries: Vec<Entry> = read
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                return None;
            }
            // Follow symlinks, so a linked folder can be opened.
            let dir = entry.path().is_dir();
            let ext = Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase);
            let wanted = match want {
                Want::Theme => ext.as_deref() == Some("toml"),
                Want::Wallpaper => ext.as_deref() == Some("png"),
            };
            (dir || wanted && entry.path().is_file()).then_some(Entry { name, dir })
        })
        .take(5000)
        .collect();
    entries.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sandbox(PathBuf);
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn sandbox(name: &str) -> Sandbox {
        let dir = std::env::temp_dir().join(format!("nts-import-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Sandbox(dir)
    }

    const NORD: &str = "name = \"Niru Nord\"\nbackground = \"2e3440\"\nforeground = \"eceff4\"\nmuted = \"9aa5b1\"\nborder = \"3b4252\"\nfocus = \"88c0d0\"\n";

    #[test]
    fn a_theme_file_is_previewed_before_import() {
        let preview = parse_theme(NORD).unwrap();
        assert_eq!(preview.name, "Niru Nord");
        assert_eq!(preview.colors[0], 0x2e3440);
        assert_eq!(preview.colors[4], 0x88c0d0);
        let bad = NORD.replace("88c0d0", "#88c0d0");
        assert!(parse_theme(&bad).unwrap_err().contains("focus"));
        let extra = format!("{NORD}surprise = 1\n");
        assert!(parse_theme(&extra)
            .unwrap_err()
            .starts_with("not a NIRUX theme"));
        let missing = NORD.replace("name = \"Niru Nord\"\n", "");
        assert!(parse_theme(&missing).is_err(), "niructl needs a name");
        assert!(parse_theme("not = [toml").is_err());
    }

    #[test]
    fn paths_are_checked_as_the_user_types() {
        let dir = sandbox("paths");
        let theme = dir.0.join("mine.toml");
        fs::write(&theme, NORD).unwrap();
        let (status, preview) = check_theme(&theme.display().to_string());
        assert_eq!(status, PathStatus::Found);
        assert_eq!(status.label(), "✓ found");
        assert_eq!(preview.unwrap().unwrap().name, "Niru Nord");
        let (status, preview) = check_theme(&dir.0.join("absent.toml").display().to_string());
        assert_eq!(status, PathStatus::NotFound);
        assert_eq!(status.label(), "✗ not found");
        assert!(preview.is_none());
        assert_eq!(check_theme("  ").0, PathStatus::Empty);
        assert!(matches!(
            check_theme(&dir.0.display().to_string()).0,
            PathStatus::Unusable(_)
        ));

        let png = dir.0.join("wall.png");
        image::RgbImage::new(4, 4).save(&png).unwrap();
        assert_eq!(
            check_wallpaper(&png.display().to_string()),
            PathStatus::Found
        );
        let jpeg = dir.0.join("wall.jpg");
        image::RgbImage::new(4, 4).save(&jpeg).unwrap();
        let PathStatus::Unusable(why) = check_wallpaper(&jpeg.display().to_string()) else {
            panic!("a JPEG must be refused up front");
        };
        assert!(why.contains("PNG"), "{why}");
        // A PNG name on JPEG bytes is still not a PNG.
        let renamed = dir.0.join("fake.png");
        fs::copy(&jpeg, &renamed).unwrap();
        assert!(!check_wallpaper(&renamed.display().to_string()).ok());
        assert_eq!(check_wallpaper(""), PathStatus::Empty);
    }

    #[test]
    fn the_browse_list_shows_folders_and_matching_files() {
        let dir = sandbox("browse");
        fs::create_dir_all(dir.0.join("Themes")).unwrap();
        fs::create_dir_all(dir.0.join(".hidden")).unwrap();
        for name in ["b.toml", "A.TOML", "c.png", "notes.txt", ".x.toml"] {
            fs::write(dir.0.join(name), b"x").unwrap();
        }
        let themes = list_dir(&dir.0, Want::Theme).unwrap();
        let names: Vec<_> = themes.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Themes", "A.TOML", "b.toml"]);
        assert!(themes[0].dir);
        let walls = list_dir(&dir.0, Want::Wallpaper).unwrap();
        let names: Vec<_> = walls.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Themes", "c.png"]);
        assert!(list_dir(&dir.0.join("absent"), Want::Theme).is_err());
    }

    #[test]
    fn paths_are_shown_relative_to_home() {
        let home = Path::new("/home/u");
        assert_eq!(
            display_path(Path::new("/home/u/Downloads/x.toml"), Some(home)),
            "~/Downloads/x.toml"
        );
        assert_eq!(display_path(home, Some(home)), "~");
        assert_eq!(display_path(Path::new("/etc/x"), Some(home)), "/etc/x");
        let dir = sandbox("start");
        assert_eq!(start_dir(Some(&dir.0)), dir.0);
        fs::create_dir_all(dir.0.join("Downloads")).unwrap();
        assert_eq!(start_dir(Some(&dir.0)), dir.0.join("Downloads"));
    }
}
