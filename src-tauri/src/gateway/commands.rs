//! Gateway commands that are not Store reads.
//!
//! Today that is one thing: the server's own filesystem, so a client whose
//! native folder dialog would produce a path on the *client's* machine can pick
//! a folder that actually exists here (plan D6).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Lists the folders below `path`, for the remote project picker.
///
/// `path` omitted or null starts at the server's home directory; a relative path
/// is refused rather than resolved against the gateway's own working directory,
/// because a picker that silently lands somewhere else is worse than an error.
/// Only folders are returned: a file cannot be opened as a project.
pub fn list_dirs(path: Option<String>, show_hidden: bool) -> Result<Value, String> {
    let resolved = match path {
        Some(path) => PathBuf::from(path),
        None => home()?,
    };
    if !resolved.is_absolute() {
        return Err(format!("not_a_directory: {}", text(&resolved)));
    }
    if !resolved.is_dir() {
        return Err(format!("not_a_directory: {}", text(&resolved)));
    }
    Ok(json!({
        "path": text(&resolved),
        "parent": resolved.parent().map(text),
        "entries": entries(&resolved, show_hidden)?,
    }))
}

/// The path as the client should see it: no trailing separator. Entry names are
/// joined onto this string, so `/srv/` and `/` would otherwise produce `//`.
fn text(path: &Path) -> String {
    let mut text = path.display().to_string();
    // A Windows drive root (`C:\`) ends in its own separator and must keep it.
    while text.len() > 1 && text.ends_with(std::path::MAIN_SEPARATOR) && !text.ends_with(':') {
        text.pop();
    }
    text
}

/// The starting point when the client sends no path.
fn home() -> Result<PathBuf, String> {
    // `HOME` is the unix answer; `USERPROFILE` keeps the gateway buildable and
    // useful on Windows, where the desktop app also runs.
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "no_home_directory: neither HOME nor USERPROFILE is set".to_string())
}

/// The folder entries of `dir`, sorted case-insensitively so the client can
/// render the list exactly as received.
fn entries(dir: &Path, show_hidden: bool) -> Result<Vec<Value>, String> {
    // A directory that exists but cannot be read is a real error, with the path
    // in it so the client shows which one.
    let read = std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let mut entries = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        // `metadata` follows a symlink, so a link to a folder is offered like the
        // folder it points at; anything unreadable or not a folder is skipped.
        if entry.metadata().map(|meta| meta.is_dir()).unwrap_or(false) {
            entries.push(json!({ "name": name, "isDir": true }));
        }
    }
    entries.sort_by(|a, b| {
        let a = a["name"].as_str().unwrap_or_default();
        let b = b["name"].as_str().unwrap_or_default();
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_is_refused_rather_than_resolved() {
        let error = list_dirs(Some("src".into()), false).unwrap_err();
        assert_eq!(error, "not_a_directory: src");
    }

    #[test]
    fn a_file_is_not_a_directory() {
        // Absolute and real, so the refusal is about the kind of path and not
        // about it being relative. `tempfile` is gateway-only; `--lib` compiles
        // this module without the feature.
        let file = std::env::current_dir().unwrap().join("Cargo.toml");
        let error = list_dirs(Some(file.to_string_lossy().into_owned()), false).unwrap_err();
        assert_eq!(error, format!("not_a_directory: {}", file.display()));
    }

    #[test]
    fn an_empty_path_is_not_a_directory() {
        // Neither absolute nor a request for the default: say so instead of
        // quietly listing somewhere the caller did not ask for.
        let error = list_dirs(Some(String::new()), false).unwrap_err();
        assert_eq!(error, "not_a_directory: ");
    }

    #[cfg(unix)]
    #[test]
    fn the_root_is_its_own_path_and_has_no_parent() {
        let listing = list_dirs(Some("/".into()), false).unwrap();
        assert_eq!(listing["path"], "/");
        // The picker's "up" affordance has nothing to point at above the root.
        assert_eq!(listing["parent"], serde_json::Value::Null);
    }
}
