//! Path presentation shared by frontends and language-service messages.
//! Formatting never changes the path used for file operations or protocol data.

use std::{
    fmt,
    path::{MAIN_SEPARATOR, Path, PathBuf},
    sync::OnceLock,
};

/// Display paths inside the current user's home as `~` or `~/relative/path`.
/// Other absolute paths and relative paths keep their existing spelling.
/// Home discovery is cached; formatting does not read the filesystem.
pub fn display(path: &Path) -> DisplayPath<'_> {
    static HOME_DIRECTORY: OnceLock<Option<PathBuf>> = OnceLock::new();
    DisplayPath {
        path,
        home: HOME_DIRECTORY.get_or_init(std::env::home_dir).as_deref(),
    }
}

pub struct DisplayPath<'a> {
    path: &'a Path,
    home: Option<&'a Path>,
}

impl fmt::Display for DisplayPath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let relative = self
            .home
            .filter(|home| home.is_absolute())
            .and_then(|home| self.path.strip_prefix(home).ok());
        match relative {
            Some(relative) if relative.as_os_str().is_empty() => f.write_str("~"),
            Some(relative) => write!(f, "~{MAIN_SEPARATOR}{}", relative.display()),
            None => self.path.display().fmt(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_abbreviation_uses_components_and_preserves_other_paths() {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        for username in ["sean", "alex", "名前"] {
            let home = Path::new(root).join("homes").join(username);
            let nested = home.join("code").join("a file.rs");
            let sibling = home
                .with_file_name(format!("{username}-other"))
                .join("file.rs");
            for (path, expected) in [
                (home.clone(), "~".to_owned()),
                (
                    nested,
                    format!("~{MAIN_SEPARATOR}code{MAIN_SEPARATOR}a file.rs"),
                ),
                (sibling.clone(), sibling.display().to_string()),
                (PathBuf::from("relative/file.rs"), "relative/file.rs".into()),
                (PathBuf::from("~/file.rs"), "~/file.rs".into()),
            ] {
                assert_eq!(
                    DisplayPath {
                        path: &path,
                        home: Some(&home)
                    }
                    .to_string(),
                    expected
                );
                for home in [None, Some(Path::new("")), Some(Path::new("relative"))] {
                    assert_eq!(
                        DisplayPath { path: &path, home }.to_string(),
                        path.display().to_string()
                    );
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_keep_the_same_lossy_display_after_abbreviation() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
        let home = Path::new(OsStr::from_bytes(b"/home/user\xff"));
        let path = home.join(OsStr::from_bytes(b"file\xfe.rs"));
        assert_eq!(
            DisplayPath {
                path: &path,
                home: Some(home)
            }
            .to_string(),
            "~/file�.rs"
        );
    }
}
