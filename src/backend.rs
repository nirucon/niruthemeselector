//! The canonical theme catalogue and management commands owned by niructl.

use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    process::{Command, Output},
};

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
    let bin = niructl()?;
    let out = Command::new(&bin)
        .args(args)
        .output()
        .map_err(|error| format!("cannot run {}: {error}", bin.display()))?;
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
    run(&["theme", id]).map(|_| ())
}

fn expand_home(path: &str) -> PathBuf {
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
    run(&refs).map(|_| ())
}

pub fn remove(id: &str) -> Result<(), String> {
    run(&["theme-remove", id]).map(|_| ())
}

pub fn hide(id: &str, hide: bool) -> Result<(), String> {
    run(&[if hide { "theme-hide" } else { "theme-show" }, id]).map(|_| ())
}

pub fn status_text() -> Result<String, String> {
    let catalog = catalog()?;
    let mut text = format!(
        "active={}\nappearance_overrides={}\nfollow_theme={}\ntheme_dir={}\n",
        catalog.active, catalog.appearance_overrides, catalog.follow_theme, catalog.theme_dir
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
    }

    #[test]
    fn expands_home_paths() {
        std::env::set_var("HOME", "/tmp/theme-home");
        assert_eq!(expand_home("~/x"), PathBuf::from("/tmp/theme-home/x"));
        assert_eq!(expand_home("/absolute"), PathBuf::from("/absolute"));
    }
}
