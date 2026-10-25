//! Where a cached download (or one file of it) is on disk, for the Cache
//! window's "View location" (the desktop shell reveals it in the file
//! manager).
//!
//! The path comes ONLY from the engine's own data: the torrent's output folder
//! and its file list, addressed by infohash and file index. Nothing from the
//! web is ever a path. And whatever the torrent's metadata says, the answer is
//! confined to the cache root: a file name with `..` in it, an absolute one,
//! or a folder inside the cache that is a junction / symlink to somewhere else
//! is refused, loudly, rather than revealed.

use std::path::{Component, Path, PathBuf};

use crate::engine::Engine;
use crate::torrent::is_valid_infohash;

/// What to show in the file manager.
#[derive(Debug, PartialEq, Eq)]
pub enum Location {
    /// The item exists: open its folder with it selected.
    Item(PathBuf),
    /// The file is not on disk (not downloaded, or not chosen for download):
    /// open the folder it would be in, the nearest one that exists.
    Folder(PathBuf),
}

/// Why there is nothing to show. `Display` is a plain sentence for a person.
#[derive(Debug, PartialEq, Eq)]
pub enum LocationError {
    BadInfoHash,
    NotInCache,
    NoSuchFile,
    /// The resolved path is outside the cache root (`..`, an absolute name,
    /// or a link out). Carries the technical detail for the log.
    OutsideCache(String),
    /// The cache root itself could not be read.
    Unreadable(String),
}

impl std::fmt::Display for LocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadInfoHash => write!(f, "That is not a download Rillio knows."),
            Self::NotInCache => write!(f, "This download is not in the cache any more."),
            Self::NoSuchFile => write!(f, "This download has no such file."),
            Self::OutsideCache(_) => {
                write!(f, "Its files point outside the cache folder, so Rillio will not open them.")
            }
            Self::Unreadable(_) => write!(f, "Rillio could not read the cache folder."),
        }
    }
}

/// `relative` (a torrent's file path, `/`-separated) joined under `base`,
/// refusing anything that is not a plain relative path: an absolute path, a
/// drive or UNC prefix, or a `..` would leave `base` by construction.
pub fn join_relative(base: &Path, relative: &str) -> Result<PathBuf, LocationError> {
    let mut out = base.to_path_buf();
    for part in relative.split(['/', '\\']).filter(|part| !part.is_empty()) {
        let piece = Path::new(part);
        let mut components = piece.components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(name)), None) => out.push(name),
            (Some(Component::CurDir), None) => {}
            _ => {
                return Err(LocationError::OutsideCache(format!(
                    "the file path {relative:?} has a component {part:?} that is not a plain name"
                )))
            }
        }
    }
    if Path::new(relative).is_absolute() || relative.starts_with(['/', '\\']) {
        return Err(LocationError::OutsideCache(format!("the file path {relative:?} is absolute")));
    }
    Ok(out)
}

/// `path`, resolved on disk (links followed), if it is inside `root`
/// (resolved the same way). `path` must exist.
///
/// TENSION (decided 2026-10-25, Codex review of feat/cache-cards-clarity):
/// this checks the path at one moment and the file manager resolves it again
/// a moment later. A process that can already write inside the cache could
/// swap a checked folder for a junction in between and steer the reveal
/// outside the cache. Accepted, not fixed: it needs a local writer the cache
/// already trusts, and since a reveal only ever SELECTS an item (never opens
/// or runs one, see the shell's reveal_cache_item), the worst case is showing
/// an outside folder. Closing it would mean holding handles on the root and
/// every ancestor through the shell call.
pub fn confine(root: &Path, path: &Path) -> Result<PathBuf, LocationError> {
    let root = std::fs::canonicalize(root)
        .map_err(|e| LocationError::Unreadable(format!("{}: {e}", root.display())))?;
    let real = std::fs::canonicalize(path)
        .map_err(|e| LocationError::Unreadable(format!("{}: {e}", path.display())))?;
    if real.starts_with(&root) {
        Ok(real)
    } else {
        Err(LocationError::OutsideCache(format!(
            "{} resolves to {}, outside the cache root {}",
            path.display(),
            real.display(),
            root.display()
        )))
    }
}

/// The location of `target` under `root`: the item itself when it exists, or
/// the nearest existing DIRECTORY above it (never above `root`). Confined
/// either way. A file standing where a folder is expected (a hostile
/// `payload.exe/missing.mkv` layout) is skipped, never returned as a Folder.
///
/// TENSION (accepted, review pass 2 of feat/cache-cards-clarity): the shell
/// only ever SELECTS a location, so a Folder is shown selected inside ITS
/// parent: revealing a missing file under `Pack/` opens the cache root with
/// `Pack` selected, and a missing torrent folder opens the root's parent with
/// the cache root selected, one level above what a person might expect.
/// Display only, nothing runs; opening the folder itself would go through
/// the shell's handler for it, which is the thing selecting avoids.
pub fn locate_path(root: &Path, target: &Path) -> Result<Location, LocationError> {
    if target.exists() {
        return confine(root, target).map(Location::Item);
    }
    let mut folder = target.parent();
    while let Some(candidate) = folder {
        if candidate.is_dir() {
            let real = confine(root, candidate)?;
            // Checked again on the resolved path: a link can name a file.
            if real.is_dir() {
                return Ok(Location::Folder(real));
            }
        }
        if candidate == root {
            break;
        }
        folder = candidate.parent();
    }
    confine(root, root).map(Location::Folder)
}

/// A path the file manager accepts. `canonicalize` on Windows yields verbatim
/// paths (`\\?\C:\...`, `\\?\UNC\server\share\...`), which Explorer does not.
pub fn display_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

impl Engine {
    /// Where a cached download is: `file_idx` names one file of it, `None`
    /// the download as a whole (a single-file torrent's file, a multi-file
    /// torrent's own folder). Only for torrents the engine runs: one set
    /// aside has no file list to resolve against.
    pub fn locate(&self, info_hash: &str, file_idx: Option<usize>) -> Result<Location, LocationError> {
        if !is_valid_infohash(info_hash) {
            return Err(LocationError::BadInfoHash);
        }
        let info_hash = info_hash.to_lowercase();
        let handle = self.get(&info_hash).ok_or(LocationError::NotInCache)?;
        let folder = self
            .output_folders()
            .remove(&info_hash)
            .ok_or(LocationError::NotInCache)?;
        let root = self.cache_root();
        let files = Engine::files(&handle);
        let target = match (file_idx, files.len()) {
            (Some(idx), _) => {
                let file = files.get(idx).ok_or(LocationError::NoSuchFile)?;
                join_relative(&folder, &file.path)?
            }
            (None, 1) => join_relative(&folder, &files[0].path)?,
            // A multi-file torrent is its own folder (librqbit lays it out
            // under the torrent's name).
            (None, _) => folder.clone(),
        };
        // The output folder itself is engine data too, and is confined like
        // everything else: a session that names a folder outside the cache
        // is not revealed either.
        locate_path(root, &target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rillio-location-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_plain_relative_path_joins_under_the_base() {
        let base = Path::new("cache");
        assert_eq!(join_relative(base, "Pack/S01E01.mkv").unwrap(), base.join("Pack").join("S01E01.mkv"));
        assert_eq!(join_relative(base, "./a.mkv").unwrap(), base.join("a.mkv"));
    }

    #[test]
    fn dot_dot_and_absolute_paths_are_refused() {
        let base = Path::new("cache");
        let mut bad_paths = vec!["../outside.mkv", "Pack/../../outside.mkv", "/etc/passwd", r"\Windows\x"];
        // Drive forms only mean something on Windows ("C:" is a legal name elsewhere).
        if cfg!(windows) {
            bad_paths.extend([r"C:\Windows\x", "C:x", r"\\server\share\x"]);
        }
        for bad in bad_paths {
            assert!(
                matches!(join_relative(base, bad), Err(LocationError::OutsideCache(_))),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn an_existing_file_is_revealed_selected() {
        let root = temp("item");
        std::fs::create_dir_all(root.join("Pack")).unwrap();
        std::fs::write(root.join("Pack").join("a.mkv"), b"x").unwrap();
        let found = locate_path(&root, &root.join("Pack").join("a.mkv")).unwrap();
        assert_eq!(found, Location::Item(std::fs::canonicalize(root.join("Pack").join("a.mkv")).unwrap()));
    }

    #[test]
    fn a_file_not_on_disk_reveals_its_nearest_folder() {
        let root = temp("missing");
        std::fs::create_dir_all(root.join("Pack")).unwrap();
        let found = locate_path(&root, &root.join("Pack").join("Season 1").join("e04.mkv")).unwrap();
        assert_eq!(found, Location::Folder(std::fs::canonicalize(root.join("Pack")).unwrap()));
        // Not even its torrent folder exists yet: the cache root itself.
        let found = locate_path(&root, &root.join("Other").join("x.mkv")).unwrap();
        assert_eq!(found, Location::Folder(std::fs::canonicalize(&root).unwrap()));
    }

    #[test]
    fn a_path_outside_the_root_is_refused() {
        let root = temp("root");
        let outside = temp("outside");
        std::fs::write(outside.join("x.mkv"), b"x").unwrap();
        assert!(matches!(locate_path(&root, &outside.join("x.mkv")), Err(LocationError::OutsideCache(_))));
    }

    /// A folder inside the cache that links OUT of it (a junction on
    /// Windows, a symlink elsewhere) must not be followed out.
    #[test]
    fn a_link_inside_the_cache_pointing_outside_is_refused() {
        let root = temp("link-root");
        let outside = temp("link-outside");
        std::fs::write(outside.join("secret.mkv"), b"x").unwrap();
        let link = root.join("Pack");
        #[cfg(windows)]
        {
            // A directory junction needs no privilege (a symlink does).
            let status = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&link)
                .arg(&outside)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "mklink /J failed");
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let through = link.join("secret.mkv");
        assert!(through.exists(), "the link resolves");
        assert!(matches!(locate_path(&root, &through), Err(LocationError::OutsideCache(_))));
        // And a missing file behind it: its folder is the link, still refused.
        assert!(matches!(locate_path(&root, &link.join("nope.mkv")), Err(LocationError::OutsideCache(_))));
    }

    /// A FILE never stands in for a missing folder: `payload.exe/missing.mkv`
    /// with `payload.exe` an existing file must not make the file manager
    /// "open the folder" `payload.exe` (which would run it).
    #[test]
    fn a_file_in_place_of_a_missing_folder_is_never_the_folder() {
        let root = temp("file-ancestor");
        std::fs::write(root.join("payload.exe"), b"MZ").unwrap();
        std::fs::write(root.join("shortcut.lnk"), b"L").unwrap();
        for ancestor in ["payload.exe", "shortcut.lnk"] {
            let found = locate_path(&root, &root.join(ancestor).join("missing.mkv")).unwrap();
            assert_eq!(found, Location::Folder(std::fs::canonicalize(&root).unwrap()), "{ancestor}");
        }
    }

    /// Commas, spaces and a leading dash are ordinary file names here: they
    /// come back as exactly that item, to be selected (never parsed).
    #[test]
    fn awkward_names_resolve_to_exactly_that_item() {
        let root = temp("awkward");
        std::fs::create_dir_all(root.join("Pack, with comma")).unwrap();
        for name in ["one,two.mkv", "-dash first.mkv", "a b , c.mkv", "payload.exe", "shortcut.lnk"] {
            std::fs::write(root.join("Pack, with comma").join(name), b"x").unwrap();
            let target = join_relative(&root, &format!("Pack, with comma/{name}")).unwrap();
            let found = locate_path(&root, &target).unwrap();
            assert_eq!(
                found,
                Location::Item(std::fs::canonicalize(root.join("Pack, with comma").join(name)).unwrap()),
                "{name}"
            );
        }
    }

    #[test]
    fn verbatim_prefixes_are_dropped_for_the_file_manager() {
        assert_eq!(display_path(Path::new(r"\\?\C:\cache\a.mkv")), PathBuf::from(r"C:\cache\a.mkv"));
        assert_eq!(display_path(Path::new(r"\\?\UNC\nas\share\a.mkv")), PathBuf::from(r"\\nas\share\a.mkv"));
        assert_eq!(display_path(Path::new("/home/x/a.mkv")), PathBuf::from("/home/x/a.mkv"));
    }

    #[test]
    fn errors_read_as_plain_sentences() {
        assert_eq!(LocationError::NotInCache.to_string(), "This download is not in the cache any more.");
        assert!(!LocationError::OutsideCache("detail".into()).to_string().contains("detail"));
    }
}
