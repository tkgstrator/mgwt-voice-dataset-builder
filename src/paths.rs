//! No-follow directory capabilities and output-path validation.

#[cfg(windows)]
#[path = "paths_windows.rs"]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(not(windows))]
mod unix {

    use crate::error::AppError;
    use rustix::fs::{self, AtFlags, Mode, OFlags};
    use std::{
        ffi::{OsStr, OsString},
        os::fd::OwnedFd,
        path::{Component, Path, PathBuf},
    };

    const DIR_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);

    fn absolute_components(path: &Path) -> Result<(PathBuf, Vec<OsString>), AppError> {
        if path.as_os_str().is_empty() {
            return Err(AppError::validation("empty path"));
        }
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| AppError::validation(format!("current directory: {e}")))?
                .join(path)
        };
        let mut parts = Vec::new();
        for component in absolute.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(part) => parts.push(part.to_os_string()),
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(AppError::validation("path traversal is not allowed"));
                }
            }
        }
        Ok((absolute, parts))
    }

    fn open_components(
        parts: &[OsString],
        excluded_dir: Option<&OwnedFd>,
    ) -> Result<OwnedFd, AppError> {
        let excluded_stat = excluded_dir
            .map(|fd| {
                fs::fstat(fd)
                    .map_err(|e| AppError::validation(format!("inspect app directory: {e}")))
            })
            .transpose()?;
        let mut dir = fs::openat(fs::CWD, "/", DIR_FLAGS, Mode::empty())
            .map_err(|e| AppError::validation(format!("open filesystem root: {e}")))?;
        if let Some(app_stat) = &excluded_stat {
            let stat = fs::fstat(&dir)
                .map_err(|e| AppError::validation(format!("inspect output ancestor: {e}")))?;
            if (stat.st_dev, stat.st_ino) == (app_stat.st_dev, app_stat.st_ino) {
                return Err(AppError::validation("output path is inside the app"));
            }
        }
        for part in parts {
            dir = fs::openat(&dir, part, DIR_FLAGS, Mode::empty())
                .map_err(|e| AppError::validation(format!("open path component {part:?}: {e}")))?;
            if let Some(app_stat) = &excluded_stat {
                let stat = fs::fstat(&dir)
                    .map_err(|e| AppError::validation(format!("inspect output ancestor: {e}")))?;
                if (stat.st_dev, stat.st_ino) == (app_stat.st_dev, app_stat.st_ino) {
                    return Err(AppError::validation("output path is inside the app"));
                }
            }
        }
        Ok(dir)
    }

    /// An app directory opened through no-follow descriptor-relative traversal.
    #[derive(Debug)]
    pub struct ValidatedAppPath {
        dir: OwnedFd,
        absolute: PathBuf,
    }

    impl ValidatedAppPath {
        pub fn new(path: &Path) -> Result<Self, AppError> {
            let (absolute, parts) = absolute_components(path)?;
            if parts.is_empty() {
                return Err(AppError::validation("app path must name a directory"));
            }
            let dir = open_components(&parts, None)?;
            Ok(Self { dir, absolute })
        }

        pub fn dir(&self) -> &OwnedFd {
            &self.dir
        }
        pub fn absolute(&self) -> &Path {
            &self.absolute
        }
    }

    /// A missing literal output leaf with its existing parent held open.
    #[derive(Debug)]
    pub struct ValidatedOutputPath {
        parent: OwnedFd,
        leaf: OsString,
        requested_parent: PathBuf,
    }

    impl ValidatedOutputPath {
        pub fn new(path: &Path, app: &ValidatedAppPath) -> Result<Self, AppError> {
            let (absolute, parts) = absolute_components(path)?;
            let (leaf, parents) = parts
                .split_last()
                .ok_or_else(|| AppError::validation("output path must name a directory"))?;
            let parent = open_components(parents, Some(app.dir()))?;
            match fs::statat(&parent, leaf, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(_) => return Err(AppError::validation("output leaf already exists")),
                Err(rustix::io::Errno::NOENT) => {}
                Err(e) => return Err(AppError::validation(format!("inspect output leaf: {e}"))),
            }
            Ok(Self {
                parent,
                leaf: leaf.clone(),
                requested_parent: absolute.parent().unwrap_or(Path::new("/")).to_path_buf(),
            })
        }

        pub fn parent(&self) -> &OwnedFd {
            &self.parent
        }
        pub fn leaf(&self) -> &OsStr {
            &self.leaf
        }
        pub fn requested_parent(&self) -> &Path {
            &self.requested_parent
        }
    }

    /// Reject equal paths and either direction of nesting before any output work.
    pub fn validate_separation(app: &Path, output: &Path) -> Result<(), AppError> {
        let (_, app_parts) = absolute_components(app)?;
        let (_, output_parts) = absolute_components(output)?;
        if app_parts.starts_with(&output_parts) || output_parts.starts_with(&app_parts) {
            return Err(AppError::validation("app and output paths overlap"));
        }
        Ok(())
    }
}

#[cfg(not(windows))]
pub use unix::*;
