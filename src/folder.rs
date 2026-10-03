//! A folder, as the person may see it: what is in it, and what they may do
//! there, asked of the system rather than guessed.
//!
//! The dialog runs as the person, so listing, making a folder, renaming and
//! deleting are their own acts, which KACS checks as for any program of
//! theirs. What they may do is found before it is offered: AccessCheck of
//! their token against the folder's descriptor, and the entry's (KACS:
//! adding a file or a folder is the folder's right to give, deleting is
//! the entry's DELETE or the folder's DELETE_CHILD). Where a descriptor
//! can't be read, nothing can be told, and the act is offered for the
//! system to decide.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use peios::access::AccessCheck;
use peios::file::{File, FileAccess, SecInfo};
use peios::security::AccessMask;

const EACCES: i32 = 13;
const ENOENT: i32 = 2;
const EEXIST: i32 = 17;
const ENOTDIR: i32 = 20;

/// One thing in a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// A folder, or a link to one.
    pub folder: bool,
    pub size: u64,
    pub changed: Option<SystemTime>,
}

/// What is in the folder at `path`, folders first, then by name; or why it
/// can't be listed. Names starting with a dot are left out unless `hidden`.
pub fn list(path: &Path, hidden: bool) -> Result<Vec<Entry>, String> {
    let read = std::fs::read_dir(path).map_err(|e| match e.raw_os_error() {
        Some(EACCES) => "You may not list this folder.".to_string(),
        Some(ENOENT) => "There is no folder here.".to_string(),
        Some(ENOTDIR) => "This is not a folder.".to_string(),
        _ => format!("This folder could not be listed: {e}."),
    })?;
    let mut entries: Vec<Entry> = read
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !hidden && name.starts_with('.') {
                return None;
            }
            // A link is shown as what it leads to, where that can be seen.
            let metadata = std::fs::metadata(entry.path()).or_else(|_| entry.metadata()).ok();
            Some(Entry {
                name,
                folder: metadata.as_ref().is_some_and(std::fs::Metadata::is_dir),
                size: metadata.as_ref().map_or(0, std::fs::Metadata::len),
                changed: metadata.and_then(|metadata| metadata.modified().ok()),
            })
        })
        .collect();
    entries.sort_by_cached_key(|entry| (!entry.folder, entry.name.to_lowercase()));
    Ok(entries)
}

/// What the person may do in a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct May {
    pub add_file: bool,
    pub add_folder: bool,
    pub delete_child: bool,
}

impl May {
    /// Everything, for a folder whose descriptor can't be read: the system
    /// decides when it is asked.
    pub const UNTOLD: May = May { add_file: true, add_folder: true, delete_child: true };
}

/// What the person may do in the folder at `path`.
pub fn may(path: &Path) -> May {
    match granted(path) {
        Some(granted) => May {
            add_file: granted.contains(FileAccess::ADD_FILE),
            add_folder: granted.contains(FileAccess::ADD_SUBDIRECTORY),
            delete_child: granted.contains(FileAccess::DELETE_CHILD),
        },
        None => May::UNTOLD,
    }
}

/// Whether the person may delete (or rename) the entry at `path`, in a
/// folder where they may do `folder`: its own DELETE, or the folder's
/// DELETE_CHILD.
pub fn may_delete(path: &Path, folder: May) -> bool {
    folder.delete_child || granted(path).is_none_or(|granted| granted.contains(FileAccess::DELETE))
}

/// What the person's token is granted on what is at `path`, as KACS says,
/// or `None` where its descriptor can't be read.
fn granted(path: &Path) -> Option<FileAccess> {
    let sd = peios::file::get_sd(None, path, SecInfo::OWNER | SecInfo::GROUP | SecInfo::DACL, 0).ok()?;
    let decision = AccessCheck::new(&sd, AccessMask::MAXIMUM_ALLOWED, File::generic_mapping()).check().ok()?;
    Some(FileAccess::from_bits_truncate(decision.granted.bits()))
}

/// A name the person typed, as one name in a folder, or why it isn't.
pub fn name(typed: &str) -> Result<&str, String> {
    let name = typed.trim();
    if name.is_empty() {
        return Err("Type a name.".into());
    }
    if name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err("A name can't be . or .., or have / in it.".into());
    }
    Ok(name)
}

/// Makes the folder `name` in `folder`.
pub fn make_folder(folder: &Path, name: &str) -> Result<PathBuf, String> {
    let path = folder.join(name);
    std::fs::create_dir(&path).map_err(|e| refused(&e, "make a folder here"))?;
    Ok(path)
}

/// Renames `from` in `folder` to `to`, never over something already there.
pub fn rename(folder: &Path, from: &str, to: &str) -> Result<(), String> {
    let target = folder.join(to);
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err(format!("There is something called {to} here already."));
    }
    std::fs::rename(folder.join(from), target).map_err(|e| refused(&e, "rename it"))
}

/// Deletes what is at `path`, and, for a folder, everything in it.
pub fn delete(path: &Path) -> Result<(), String> {
    let deleted = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        _ => std::fs::remove_file(path),
    };
    deleted.map_err(|e| refused(&e, "delete it"))
}

fn refused(e: &std::io::Error, what: &str) -> String {
    match e.raw_os_error() {
        Some(EACCES) => format!("You may not {what}."),
        Some(EEXIST) => "There is something of that name here already.".into(),
        _ => format!("Could not {what}: {e}."),
    }
}

/// A size, in the unit that suits it.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return if bytes == 1 { "1 byte".into() } else { format!("{bytes} bytes") };
    }
    let mut amount = bytes as f64 / 1000.0;
    let mut unit = 0;
    while amount >= 1000.0 && unit < UNITS.len() - 1 {
        amount /= 1000.0;
        unit += 1;
    }
    if amount < 10.0 { format!("{amount:.1} {}", UNITS[unit]) } else { format!("{amount:.0} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gxwi-file-dialog-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_folder_lists_its_folders_first_and_its_hidden_names_only_when_asked() {
        let dir = scratch("list");
        std::fs::write(dir.join("b.txt"), "bb").unwrap();
        std::fs::write(dir.join("A.json"), "").unwrap();
        std::fs::write(dir.join(".profile"), "").unwrap();
        std::fs::create_dir(dir.join("zeta")).unwrap();
        let names = |hidden| list(&dir, hidden).unwrap().into_iter().map(|e| e.name).collect::<Vec<_>>();
        assert_eq!(names(false), ["zeta", "A.json", "b.txt"]);
        assert_eq!(names(true), ["zeta", ".profile", "A.json", "b.txt"]);
        assert_eq!(list(&dir.join("b.txt"), false).unwrap_err(), "This is not a folder.");
        assert_eq!(list(&dir.join("nowhere"), false).unwrap_err(), "There is no folder here.");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn folders_are_made_renamed_and_deleted() {
        let dir = scratch("acts");
        let made = make_folder(&dir, "New").unwrap();
        std::fs::write(made.join("inside"), "x").unwrap();
        assert!(make_folder(&dir, "New").unwrap_err().contains("already"));
        std::fs::write(dir.join("taken"), "").unwrap();
        assert!(rename(&dir, "New", "taken").unwrap_err().contains("already"));
        rename(&dir, "New", "Old").unwrap();
        delete(&dir.join("Old")).unwrap();
        assert!(!dir.join("Old").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_typed_name_is_one_name() {
        assert_eq!(name("  a.json "), Ok("a.json"));
        assert!(name("").is_err() && name("..").is_err() && name("a/b").is_err());
    }

    #[test]
    fn sizes_are_said_in_the_unit_that_suits_them() {
        assert_eq!(size(1), "1 byte");
        assert_eq!(size(999), "999 bytes");
        assert_eq!(size(1500), "1.5 KB");
        assert_eq!(size(25_000_000), "25 MB");
    }
}
