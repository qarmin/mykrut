//! Default-application lookup for the context-menu "Open" label.
//!
//! Flow:
//! 1. Selection changes → `refresh()` is invoked.
//! 2. If exactly one file is selected, we figure out its MIME (mime_guess
//!    by extension; falls back to the entry's stored mime), then look up
//!    the user's default app for that MIME by parsing `mimeapps.list`
//!    and walking the standard XDG application directories for the
//!    matching `.desktop` file.
//! 3. The resolved `Name=` is pushed into `AppState.default-app-name`.
//! 4. Result is cached in-memory keyed by MIME so subsequent selections
//!    of the same file type don't re-do the disk work.
//!
//! Why not shell out to `xdg-mime`: spawning a subprocess on every
//! selection change is slow (and synchronous). The on-disk format is
//! well-specified, parsing it directly costs sub-millisecond per lookup.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use slint::ComponentHandle;
use tokio::runtime::Runtime;
use tracing::debug;

use crate::state::AppStateRc;
use crate::{AppState, MainWindow};

thread_local! {
    /// Cache: MIME → Some(human-name) when known, None when we tried and
    /// couldn't resolve it. Lives for the app's lifetime; trade-off is
    /// memory (a few entries) vs avoiding repeated file walks.
    static APP_NAME_CACHE: RefCell<HashMap<String, Option<String>>> = RefCell::new(HashMap::new());
}

/// Called from selection.rs after every selection change. We don't have
/// our own Slint callback hookup — driving from selection lets the cache
/// + AppState write live entirely on the UI thread.
pub fn wire(_app: &MainWindow, _rt: &Arc<Runtime>, _state: AppStateRc) {
    // No Slint callback wiring needed; `refresh()` is the public entry.
}

/// Recompute `AppState.default-app-name` based on the current selection.
/// Empty / multi-selection → cleared. Single file → resolved name (or "").
pub fn refresh(app: &MainWindow, state: &AppStateRc) {
    let single_file_path: Option<PathBuf> = {
        let s = state.borrow();
        if s.selected.len() != 1 {
            None
        } else {
            s.selected
                .iter()
                .next()
                .and_then(|&i| s.entries.get(i))
                .filter(|e| !e.is_dir())
                .map(|e| e.path.clone())
        }
    };
    let name = single_file_path
        .as_deref()
        .and_then(default_app_name_for)
        .unwrap_or_default();
    app.global::<AppState>().set_default_app_name(name.into());
}

fn default_app_name_for(path: &Path) -> Option<String> {
    let mime = mime_for(path)?;
    // Cache hit — including negative cache (we tried last time and
    // couldn't resolve, no point re-walking).
    if let Some(hit) = APP_NAME_CACHE.with(|c| c.borrow().get(&mime).cloned()) {
        return hit;
    }
    let resolved = resolve_default_app_name(&mime);
    APP_NAME_CACHE.with(|c| c.borrow_mut().insert(mime.clone(), resolved.clone()));
    if let Some(name) = &resolved {
        debug!(mime = %mime, name = %name, "default app resolved");
    }
    resolved
}

pub fn mime_for(path: &Path) -> Option<String> {
    mime_guess::from_path(path).first().map(|m| m.essence_str().to_string())
}

/// Drop the cached default-app names. Called after the user changes a default
/// so the context-menu "Open" label reflects the new association.
pub fn invalidate_cache() {
    APP_NAME_CACHE.with(|c| c.borrow_mut().clear());
}

/// One installed application that can be offered in the "Open with" dialog.
#[derive(Clone, Debug)]
pub struct DesktopApp {
    /// `.desktop` file name, e.g. `org.gnome.gedit.desktop`.
    pub id: String,
    pub name: String,
    /// Raw `Exec=` line (field codes still present; expanded at launch time).
    pub exec: String,
    /// True when the app declares this MIME type in its `MimeType=`.
    pub recommended: bool,
}

struct DesktopEntry {
    name: String,
    exec: String,
    mime_types: Vec<String>,
    show: bool,
}

/// Parse the `[Desktop Entry]` group for the fields the picker needs. Returns
/// `None` for entries with no `Exec`.
fn parse_desktop_entry(path: &Path) -> Option<DesktopEntry> {
    let raw = std::fs::read_to_string(path).ok()?;
    let lang_pref = current_lang_prefix();
    let mut in_entry = false;
    let mut name: Option<String> = None;
    let mut localized_name: Option<String> = None;
    let mut exec: Option<String> = None;
    let mut type_app = false;
    let mut hidden = false;
    let mut mime_types: Vec<String> = Vec::new();

    for line in raw.lines() {
        let line = line.trim_end();
        if line.starts_with('[') && line.ends_with(']') {
            // Only the first [Desktop Entry] group matters; stop at the next.
            if in_entry {
                break;
            }
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "Name" => name = Some(value.to_string()),
            "Exec" => exec = Some(value.to_string()),
            "Type" => type_app = value == "Application",
            "NoDisplay" | "Hidden" => hidden |= value.eq_ignore_ascii_case("true"),
            "MimeType" => mime_types = value.split(';').filter(|s| !s.is_empty()).map(str::to_string).collect(),
            _ => {
                if let Some(rest) = key.strip_prefix("Name[")
                    && let (Some(locale), Some(pref)) = (rest.strip_suffix(']'), lang_pref.as_ref())
                    && (locale == pref || locale.starts_with(&format!("{pref}_")))
                {
                    localized_name = Some(value.to_string());
                }
            }
        }
    }

    let exec = exec?;
    if exec.trim().is_empty() {
        return None;
    }
    Some(DesktopEntry {
        name: localized_name.or(name).unwrap_or_else(|| "(unnamed)".to_string()),
        exec,
        mime_types,
        show: type_app && !hidden,
    })
}

/// Enumerate installed applications, marking those that declare `mime` as
/// recommended. Recommended apps sort first, then alphabetical. Honours XDG
/// directory precedence (first occurrence of a given `.desktop` id wins).
pub fn apps_for_mime(mime: Option<&str>) -> Vec<DesktopApp> {
    use std::collections::HashSet;
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<DesktopApp> = Vec::new();

    for dir in application_dirs() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Some(id) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            if !seen.insert(id.clone()) {
                continue; // already provided by a higher-precedence dir
            }
            let Some(de) = parse_desktop_entry(&path) else {
                continue;
            };
            if !de.show {
                continue;
            }
            let recommended = mime.is_some_and(|m| de.mime_types.iter().any(|t| t == m));
            out.push(DesktopApp {
                id,
                name: de.name,
                exec: de.exec,
                recommended,
            });
        }
    }

    out.sort_by(|a, b| {
        b.recommended
            .cmp(&a.recommended)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

/// Resolve the current default `.desktop` id for `mime`, if any.
pub fn default_desktop_id(mime: &str) -> Option<String> {
    find_default_desktop_id(mime)
}

/// Persist `id` as the default application for `mime` in
/// `$XDG_CONFIG_HOME/mimeapps.list`, preserving the rest of the file.
pub fn set_default(mime: &str, id: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    let dir = dirs::config_dir().context("no XDG config dir")?;
    let path = dir.join("mimeapps.list");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = upsert_default_app(&existing, mime, id);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(&path, updated).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Insert/replace `mime=id` under `[Default Applications]`, leaving every other
/// line of the file untouched. Appends the section if it doesn't exist.
fn upsert_default_app(content: &str, mime: &str, id: &str) -> String {
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    let new_line = format!("{mime}={id}");

    let section = lines.iter().position(|l| l.trim() == "[Default Applications]");
    match section {
        Some(start) => {
            let end = lines[start + 1..]
                .iter()
                .position(|l| {
                    let t = l.trim();
                    t.starts_with('[') && t.ends_with(']')
                })
                .map_or(lines.len(), |off| start + 1 + off);
            let existing = lines[start + 1..end]
                .iter()
                .position(|l| l.trim_start().starts_with(&format!("{mime}=")))
                .map(|off| start + 1 + off);
            match existing {
                Some(i) => lines[i] = new_line,
                None => lines.insert(end, new_line),
            }
        }
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("[Default Applications]".to_string());
            lines.push(new_line);
        }
    }

    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Launch `exec` (a `.desktop` Exec line) on `paths`, expanding the file-list
/// field codes. Unknown `%` codes are dropped. Spawns detached.
pub fn launch(exec: &str, paths: &[PathBuf]) -> anyhow::Result<()> {
    use anyhow::{Context, anyhow};
    let argv = build_argv(exec, paths);
    let (program, args) = argv.split_first().ok_or_else(|| anyhow!("empty Exec line"))?;
    std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("spawn {program}"))?;
    Ok(())
}

fn build_argv(exec: &str, paths: &[PathBuf]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tok in exec.split_whitespace() {
        match tok {
            "%f" | "%u" => {
                if let Some(p) = paths.first() {
                    out.push(p.display().to_string());
                }
            }
            "%F" | "%U" => out.extend(paths.iter().map(|p| p.display().to_string())),
            // Drop the remaining field codes (%i icon, %c name, %k path, %%, ...).
            t if t.starts_with('%') => {}
            t => out.push(t.to_string()),
        }
    }
    out
}

/// Implements (a subset of) the freedesktop mime-apps spec well enough to
/// get the user's default for common types. Order of precedence:
///
/// 1. `~/.config/mimeapps.list` `[Default Applications]`
/// 2. `~/.local/share/applications/mimeapps.list`
/// 3. `/usr/share/applications/mimeapps.list` (system default)
///
/// then resolve the resulting `foo.desktop` by walking applications dirs:
///
/// - $XDG_DATA_HOME/applications
/// - each entry of $XDG_DATA_DIRS/applications
/// - fallback /usr/local/share/applications, /usr/share/applications
fn resolve_default_app_name(mime: &str) -> Option<String> {
    let desktop_id = find_default_desktop_id(mime)?;
    let desktop_path = find_desktop_file(&desktop_id)?;
    parse_desktop_name(&desktop_path)
}

fn find_default_desktop_id(mime: &str) -> Option<String> {
    let candidates = mimeapps_list_candidates();
    for path in candidates {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(id) = parse_mimeapps_default(&raw, mime) {
            return Some(id);
        }
    }
    None
}

fn mimeapps_list_candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::config_dir() {
        out.push(home.join("mimeapps.list"));
    }
    if let Some(data_home) = dirs::data_local_dir() {
        out.push(data_home.join("applications").join("mimeapps.list"));
    }
    out.push(PathBuf::from("/usr/local/share/applications/mimeapps.list"));
    out.push(PathBuf::from("/usr/share/applications/mimeapps.list"));
    out
}

/// Parse the INI-ish mimeapps.list for the first value under
/// `[Default Applications]` matching `mime`. Values can be
/// semicolon-separated lists; we take the first id.
fn parse_mimeapps_default(raw: &str, mime: &str) -> Option<String> {
    let mut in_section = false;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_section = line == "[Default Applications]";
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == mime {
            let id = value.split(';').next()?.trim().to_string();
            if id.is_empty() {
                return None;
            }
            return Some(id);
        }
    }
    None
}

fn find_desktop_file(desktop_id: &str) -> Option<PathBuf> {
    for dir in application_dirs() {
        let p = dir.join(desktop_id);
        if p.is_file() {
            return Some(p);
        }
        // Some IDs are dash-separated like `org.gnome.gedit.desktop`; they
        // are not always reflected as nested directories but a few
        // packages do organise their .desktop files that way. Try a flat
        // probe with `-` → `/` as a best-effort fallback.
        if desktop_id.contains('-') {
            let nested = dir.join(desktop_id.replace('-', "/"));
            if nested.is_file() {
                return Some(nested);
            }
        }
    }
    None
}

fn application_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(p) = dirs::data_local_dir() {
        out.push(p.join("applications"));
    }
    let xdg_data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for d in xdg_data_dirs.split(':').filter(|s| !s.is_empty()) {
        out.push(PathBuf::from(d).join("applications"));
    }
    out.push(PathBuf::from("/usr/local/share/applications"));
    out.push(PathBuf::from("/usr/share/applications"));
    out
}

/// Pull the user-visible `Name=` (or `Name[locale]=`) from a .desktop
/// file. Doesn't fully implement locale fallback — we take the plain
/// `Name=` and only switch to a localized one if it's a strict match for
/// the LANG prefix. Good enough for the menu label.
fn parse_desktop_name(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let lang_pref = current_lang_prefix();

    let mut in_entry = false;
    let mut plain_name: Option<String> = None;
    let mut localized_name: Option<String> = None;

    for line in raw.lines() {
        let line = line.trim_end();
        if line.starts_with('[') && line.ends_with(']') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key == "Name" {
            plain_name = Some(value.to_string());
        } else if let Some(rest) = key.strip_prefix("Name[")
            && let Some(locale) = rest.strip_suffix(']')
            && let Some(pref) = &lang_pref
            && (locale == pref || locale.starts_with(&format!("{pref}_")))
        {
            localized_name = Some(value.to_string());
        }
    }
    localized_name.or(plain_name)
}

fn current_lang_prefix() -> Option<String> {
    let lang = std::env::var("LANG").ok()?;
    // "pl_PL.UTF-8" → "pl"
    let trimmed = lang.split(&['.', '_'][..]).next()?;
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_applications_section() {
        let raw = "\
[Added Associations]\n\
foo=bar.desktop\n\
[Default Applications]\n\
text/plain=gedit.desktop;\n\
image/jpeg=eog.desktop;feh.desktop;\n\
";
        assert_eq!(
            parse_mimeapps_default(raw, "text/plain").as_deref(),
            Some("gedit.desktop")
        );
        // First entry of a list wins.
        assert_eq!(
            parse_mimeapps_default(raw, "image/jpeg").as_deref(),
            Some("eog.desktop")
        );
        // Section gating: keys in [Added Associations] don't leak through.
        assert_eq!(parse_mimeapps_default(raw, "foo"), None);
    }

    #[test]
    fn parse_desktop_name_picks_plain_when_no_locale_match() {
        let dir = tempdir();
        let p = dir.join("x.desktop");
        std::fs::write(&p, "[Desktop Entry]\nName=Vim\nName[fr]=Vim FR\nExec=vim %F\n").unwrap();
        // Force LANG to something with no match.
        // SAFETY: single-threaded test setup, no concurrent env access.
        unsafe { std::env::set_var("LANG", "de_DE.UTF-8") };
        assert_eq!(parse_desktop_name(&p).as_deref(), Some("Vim"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempdir() -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("fm-test-default-app-{n:x}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn upsert_replaces_existing_default() {
        let raw = "[Default Applications]\ntext/plain=old.desktop\nimage/png=eog.desktop\n";
        let out = upsert_default_app(raw, "text/plain", "new.desktop");
        assert!(out.contains("text/plain=new.desktop"));
        assert!(out.contains("image/png=eog.desktop"), "other entries preserved");
        assert!(!out.contains("old.desktop"));
    }

    #[test]
    fn upsert_adds_section_when_missing() {
        let raw = "[Added Associations]\nfoo=bar.desktop\n";
        let out = upsert_default_app(raw, "text/plain", "gedit.desktop");
        assert!(out.contains("[Added Associations]"), "existing section kept");
        assert!(out.contains("[Default Applications]"));
        assert!(out.contains("text/plain=gedit.desktop"));
    }

    #[test]
    fn upsert_appends_within_existing_section() {
        let raw = "[Default Applications]\ntext/plain=gedit.desktop\n[Removed Associations]\nx=y.desktop\n";
        let out = upsert_default_app(raw, "image/png", "eog.desktop");
        let dflt = out.find("[Default Applications]").unwrap();
        let removed = out.find("[Removed Associations]").unwrap();
        let png = out.find("image/png=eog.desktop").unwrap();
        assert!(dflt < png && png < removed, "new entry lands inside the right section");
    }

    #[test]
    fn build_argv_expands_field_codes() {
        let p = vec![PathBuf::from("/a/b.txt"), PathBuf::from("/c d.png")];
        assert_eq!(build_argv("gimp %U", &p), vec!["gimp", "/a/b.txt", "/c d.png"]);
        assert_eq!(build_argv("xterm %f", &p), vec!["xterm", "/a/b.txt"]);
        // Unknown codes dropped; literal args kept.
        assert_eq!(
            build_argv("app -n %i %F", &p),
            vec!["app", "-n", "/a/b.txt", "/c d.png"]
        );
    }
}
