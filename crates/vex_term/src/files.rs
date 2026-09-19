//! File loading and atomic replacement. Disk checks run on save, never on draw.

use std::{
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};
use vex_core::{Document, Rope};

#[derive(Debug)]
pub struct FileState {
    path: Option<PathBuf>,
    target: Option<PathBuf>,
    existed: bool,
    saved: Rope,
    newline: &'static str,
}

impl FileState {
    pub fn load(path: Option<&Path>) -> io::Result<(Document, Self)> {
        let Some(path) = path else {
            let document = Document::default();
            let state = Self::scratch(&document);
            return Ok((document, state));
        };
        let target = resolve(path)?;
        if fs::metadata(&target).is_ok_and(|metadata| !metadata.is_file()) {
            return Err(io::Error::other("only regular files can be edited"));
        }
        let (document, existed) = match File::open(&target) {
            Ok(file) => {
                if !file.metadata()?.is_file() {
                    return Err(io::Error::other("only regular files can be edited"));
                }
                (Document::from_reader(BufReader::new(file))?, true)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (Document::default(), false),
            Err(error) => return Err(error),
        };
        let mut state = Self::scratch(&document);
        state.path = Some(path.into());
        state.target = Some(target);
        state.existed = existed;
        Ok((document, state))
    }

    pub fn scratch(document: &Document) -> Self {
        let first = document.text().line(0);
        let len = first.len_chars();
        let newline = if len >= 2 && first.char(len - 2) == '\r' && first.char(len - 1) == '\n' {
            "\r\n"
        } else if len >= 1 && first.char(len - 1) == '\r' {
            "\r"
        } else {
            "\n"
        };
        Self {
            path: None,
            target: None,
            existed: false,
            saved: document.text().clone(),
            newline,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub fn newline(&self) -> &'static str {
        self.newline
    }

    /// O(1) savepoint check. Undo/redo restores shared rope identities. An edit
    /// that recreates identical bytes without undo still counts as modified.
    pub fn is_dirty(&self, document: &Document) -> bool {
        !self.saved.is_instance(document.text())
    }

    /// Write and sync a temporary sibling, then atomically replace the target.
    /// Symlinks resolve to their target; existing permissions are retained.
    /// Without force, external changes and overwriting another file are errors.
    pub fn save(
        &mut self,
        document: &Document,
        path: Option<&Path>,
        force: bool,
    ) -> io::Result<usize> {
        let path = path
            .or(self.path())
            .ok_or_else(|| io::Error::other("no file name; use :w PATH"))?
            .to_path_buf();
        let target = resolve(&path)?;
        let current = fs::metadata(&target).map(Some).or_else(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(error)
            }
        })?;
        if let Some(metadata) = &current {
            if !metadata.is_file() {
                return Err(io::Error::other("only regular files can be written"));
            }
            if metadata.permissions().readonly() && !force {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "file is read-only; use :w! to replace it",
                ));
            }
        }
        let same_target = self.target.as_ref() == Some(&target);
        if !force && current.is_some() && (!same_target || !self.existed) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "file already exists; use :w! to overwrite it",
            ));
        }
        if !force && same_target && self.existed && current.is_none() {
            return Err(io::Error::other(
                "file was removed on disk; use :w! to recreate it",
            ));
        }
        let parent = target
            .parent()
            .ok_or_else(|| io::Error::other("file has no parent directory"))?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".vex-")
            .tempfile_in(parent)?;
        {
            let mut writer = BufWriter::new(temporary.as_file_mut());
            document.write_to(&mut writer)?;
            writer.flush()?;
        }
        if let Some(metadata) = &current {
            temporary
                .as_file()
                .set_permissions(metadata.permissions())?;
        }
        temporary.as_file().sync_all()?;
        // Check after the potentially long write, immediately before replacement.
        if !force && same_target && self.existed && !matches_disk(&target, &self.saved)? {
            return Err(io::Error::other(
                "file changed on disk; use :w! to overwrite it",
            ));
        }
        if force || current.is_some() {
            temporary.persist(&target).map_err(|error| error.error)?;
        } else {
            temporary
                .persist_noclobber(&target)
                .map_err(|error| error.error)?;
        }
        self.path = Some(path);
        self.target = Some(target);
        self.existed = true;
        self.saved = document.text().clone();
        Ok(document.text().len_bytes())
    }
}

fn resolve(path: &Path) -> io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_symlink()) {
                return Err(io::Error::other(
                    "cannot write through a dangling symbolic link",
                ));
            }
            let name = path
                .file_name()
                .ok_or_else(|| io::Error::other("missing file name"))?;
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            Ok(fs::canonicalize(parent)?.join(name))
        }
        Err(error) => Err(error),
    }
}

fn matches_disk(path: &Path, saved: &Rope) -> io::Result<bool> {
    let mut file = BufReader::new(File::open(path)?);
    if file.get_ref().metadata()?.len() != saved.len_bytes() as u64 {
        return Ok(false);
    }
    let mut buffer = [0; 8192];
    for chunk in saved.chunks() {
        for bytes in chunk.as_bytes().chunks(buffer.len()) {
            match file.read_exact(&mut buffer[..bytes.len()]) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(error) => return Err(error),
            }
            if bytes != &buffer[..bytes.len()] {
                return Ok(false);
            }
        }
    }
    Ok(file.read(&mut buffer[..1])? == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vex_core::SelectionSet;

    fn insert(document: &mut Document, text: &str) {
        let mut selections = SelectionSet::default();
        let transaction = document.replace_selections(&selections, text).unwrap();
        document.apply(transaction, &mut selections).unwrap();
    }

    #[test]
    fn saves_unicode_preserves_line_endings_and_tracks_undo_to_savepoint() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");
        fs::write(&path, "hello\r\n").unwrap();
        let (mut document, mut state) = FileState::load(Some(&path)).unwrap();
        assert_eq!(state.newline(), "\r\n");
        assert!(!state.is_dirty(&document));
        insert(&mut document, "🦀");
        assert!(state.is_dirty(&document));
        document.undo(&mut SelectionSet::default()).unwrap();
        assert!(!state.is_dirty(&document));
        document.redo(&mut SelectionSet::default()).unwrap();
        state.save(&document, None, false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "🦀hello\r\n");
        assert!(!state.is_dirty(&document));
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn external_changes_and_save_as_collisions_do_not_overwrite_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        let other = directory.path().join("other");
        fs::write(&path, "before").unwrap();
        fs::write(&other, "keep").unwrap();
        let (mut document, mut state) = FileState::load(Some(&path)).unwrap();
        insert(&mut document, "local");
        fs::write(&path, "remote").unwrap();
        assert!(state.save(&document, None, false).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "remote");
        assert!(state.is_dirty(&document));
        assert!(state.save(&document, Some(&other), false).is_err());
        assert_eq!(fs::read_to_string(&other).unwrap(), "keep");
        state.save(&document, None, true).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "localbefore");
    }

    #[test]
    fn new_files_and_failed_saves_keep_the_right_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("new file");
        let (mut document, mut state) = FileState::load(Some(&path)).unwrap();
        insert(&mut document, "new");
        assert!(!path.exists());
        assert!(
            state
                .save(&document, Some(directory.path()), false)
                .is_err()
        );
        assert!(state.is_dirty(&document));
        state.save(&document, None, false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert!(!state.is_dirty(&document));
        fs::write(&path, [0xff]).unwrap();
        assert!(FileState::load(Some(&path)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_and_permissions_survive_save() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let link = directory.path().join("link");
        fs::write(&target, "text").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&target, &link).unwrap();
        let (mut document, mut state) = FileState::load(Some(&link)).unwrap();
        insert(&mut document, "new");
        state.save(&document, None, false).unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "newtext");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}
