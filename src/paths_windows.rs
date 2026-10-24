//! Windows directory capabilities for approved input and output paths.

use crate::error::AppError;
use cap_fs_ext::DirExt;
use cap_std::{
    ambient_authority,
    fs::{Dir, MetadataExt as _},
};
use std::{
    ffi::{OsStr, OsString},
    io,
    path::{Component, Path, PathBuf, Prefix},
};

// Every Windows reparse tag is excluded, including junctions and mount points.
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

fn invalid(message: impl Into<String>) -> AppError {
    AppError::validation(message)
}

pub(crate) fn component_is_safe(component: &OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;

    let units: Vec<u16> = component.encode_wide().collect();
    if units.is_empty()
        || units.iter().any(|&unit| {
            matches!(unit, 0 | 34 | 42 | 47 | 58 | 60 | 62 | 63 | 92 | 124) || unit < 32
        })
        || matches!(units.last(), Some(32 | 46))
    {
        return false;
    }

    // Win32 resolves these names as devices even when an extension is added.
    let stem = component.to_string_lossy();
    let stem = stem
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    !matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM0"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT0"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
            | "COM¹"
            | "COM²"
            | "COM³"
            | "LPT¹"
            | "LPT²"
            | "LPT³"
    )
}

/// Normalize a drive/UNC absolute path or an ordinary path relative to the
/// current directory. Drive-relative (`C:foo`) and root-relative (`\\foo`) paths
/// have process-dependent meanings and are rejected.
fn absolute_components(path: &Path) -> Result<(PathBuf, PathBuf, Vec<OsString>), AppError> {
    if path.as_os_str().is_empty() {
        return Err(invalid("empty path"));
    }
    if (path.has_root() || matches!(path.components().next(), Some(Component::Prefix(_))))
        && !path.is_absolute()
    {
        return Err(invalid(
            "drive-relative or root-relative path is not allowed",
        ));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| invalid(format!("current directory: {e}")))?
            .join(path)
    };

    let mut components = absolute.components();
    let prefix = match components.next() {
        Some(Component::Prefix(prefix))
            if matches!(
                prefix.kind(),
                Prefix::Disk(_)
                    | Prefix::UNC(_, _)
                    | Prefix::VerbatimDisk(_)
                    | Prefix::VerbatimUNC(_, _)
            ) =>
        {
            prefix
        }
        _ => return Err(invalid("path needs a drive or UNC share")),
    };
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(invalid("path needs an absolute drive or UNC root"));
    }
    let mut root = PathBuf::from(prefix.as_os_str());
    root.push(Path::new(r"\"));
    let mut parts = Vec::new();
    for component in components {
        match component {
            Component::CurDir => {}
            Component::Normal(part) if component_is_safe(part) => parts.push(part.to_os_string()),
            _ => {
                return Err(invalid(
                    "path traversal or unsafe Windows name is not allowed",
                ));
            }
        }
    }
    Ok((absolute, root, parts))
}

fn reject_reparse(dir: &Dir) -> Result<(), AppError> {
    let metadata = dir
        .dir_metadata()
        .map_err(|e| invalid(format!("inspect directory handle: {e}")))?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid("reparse point in directory path"));
    }
    Ok(())
}

/// Compare the identity of already opened directory handles. This is also
/// used by inventory when checking that an output parent has not moved.
pub(crate) fn directory_identity(dir: &Dir) -> Result<mgwt_windows_native::FileIdentity, AppError> {
    mgwt_windows_native::file_identity(dir)
        .map_err(|e| invalid(format!("inspect directory identity: {e}")))
}

fn open_components(
    root: &Path,
    parts: &[OsString],
    excluded_dir: Option<&Dir>,
) -> Result<Dir, AppError> {
    let excluded_id = excluded_dir.map(directory_identity).transpose()?;
    let mut dir = Dir::open_ambient_dir(root, ambient_authority())
        .map_err(|e| invalid(format!("open filesystem root {root:?}: {e}")))?;
    reject_reparse(&dir)?;
    if let Some(ref excluded_id) = excluded_id {
        if &directory_identity(&dir)? == excluded_id {
            return Err(invalid("output path is inside the app"));
        }
    }
    for part in parts {
        dir = dir
            .open_dir_nofollow(Path::new(part))
            .map_err(|e| invalid(format!("open path component {part:?}: {e}")))?;
        reject_reparse(&dir)?;
        if let Some(ref excluded_id) = excluded_id {
            if &directory_identity(&dir)? == excluded_id {
                return Err(invalid("output path is inside the app"));
            }
        }
    }
    Ok(dir)
}

/// Open an existing directory through no-follow, handle-relative traversal.
pub(crate) fn open_validated_directory(
    path: &Path,
    excluded_dir: Option<&Dir>,
) -> Result<Dir, AppError> {
    let (_, root, parts) = absolute_components(path)?;
    open_components(&root, &parts, excluded_dir)
}

/// An app directory held by a no-follow directory capability.
#[derive(Debug)]
pub struct ValidatedAppPath {
    dir: Dir,
    absolute: PathBuf,
}

impl ValidatedAppPath {
    pub fn new(path: &Path) -> Result<Self, AppError> {
        let (absolute, root, parts) = absolute_components(path)?;
        if parts.is_empty() {
            return Err(invalid("app path must name a directory"));
        }
        let dir = open_components(&root, &parts, None)?;
        Ok(Self { dir, absolute })
    }

    pub fn dir(&self) -> &Dir {
        &self.dir
    }

    pub fn absolute(&self) -> &Path {
        &self.absolute
    }
}

/// A missing literal output leaf with its existing parent held open.
#[derive(Debug)]
pub struct ValidatedOutputPath {
    parent: Dir,
    leaf: OsString,
    requested_parent: PathBuf,
}

impl ValidatedOutputPath {
    pub fn new(path: &Path, app: &ValidatedAppPath) -> Result<Self, AppError> {
        let (absolute, root, parts) = absolute_components(path)?;
        if matches!(
            absolute.components().next(),
            Some(Component::Prefix(prefix))
                if matches!(prefix.kind(), Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _))
        ) {
            return Err(invalid("network output paths are not supported"));
        }
        let (leaf, parents) = parts
            .split_last()
            .ok_or_else(|| invalid("output path must name a directory"))?;
        let parent = open_components(&root, parents, Some(app.dir()))?;
        match parent.symlink_metadata(Path::new(leaf)) {
            Ok(_) => return Err(invalid("output leaf already exists")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(invalid(format!("inspect output leaf: {e}"))),
        }
        Ok(Self {
            parent,
            leaf: leaf.clone(),
            requested_parent: absolute
                .parent()
                .ok_or_else(|| invalid("output path has no parent"))?
                .to_path_buf(),
        })
    }

    pub fn parent(&self) -> &Dir {
        &self.parent
    }

    pub fn leaf(&self) -> &OsStr {
        &self.leaf
    }

    pub fn requested_parent(&self) -> &Path {
        &self.requested_parent
    }
}

fn component_eq_windows(a: &OsStr, b: &OsStr) -> bool {
    a.to_string_lossy()
        .eq_ignore_ascii_case(b.to_string_lossy().as_ref())
}

fn path_is_prefix(a: &[OsString], b: &[OsString]) -> bool {
    a.len() <= b.len()
        && a.iter()
            .zip(b)
            .all(|(left, right)| component_eq_windows(left, right))
}

/// Reject equal paths and either direction of nesting before output work.
pub fn validate_separation(app: &Path, output: &Path) -> Result<(), AppError> {
    let (_, app_root, app_parts) = absolute_components(app)?;
    let (_, output_root, output_parts) = absolute_components(output)?;
    if app_root
        .to_string_lossy()
        .eq_ignore_ascii_case(output_root.to_string_lossy().as_ref())
        && (path_is_prefix(&app_parts, &output_parts) || path_is_prefix(&output_parts, &app_parts))
    {
        return Err(invalid("app and output paths overlap"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn case_aliases_and_existing_leaf() {
        let temp = tempfile::tempdir().unwrap();
        let app_path = temp.path().join("game.app");
        fs::create_dir(&app_path).unwrap();
        let app = ValidatedAppPath::new(&app_path).unwrap();
        assert!(validate_separation(&app_path, &temp.path().join("GAME.APP/out")).is_err());
        assert!(ValidatedOutputPath::new(&temp.path().join("GAME.APP/out"), &app).is_err());
        let out = ValidatedOutputPath::new(&temp.path().join("dataset"), &app).unwrap();
        assert_eq!(out.leaf(), OsStr::new("dataset"));
        fs::create_dir(temp.path().join("occupied")).unwrap();
        assert!(ValidatedOutputPath::new(&temp.path().join("occupied"), &app).is_err());
    }

    #[test]
    fn rejects_unsafe_names_and_drive_relative_paths() {
        let temp = tempfile::tempdir().unwrap();
        for leaf in ["NUL", "COM1.txt", "trailing.", "stream:name"] {
            assert!(
                absolute_components(&temp.path().join(leaf)).is_err(),
                "{leaf}"
            );
        }
        assert!(absolute_components(Path::new(r"C:relative")).is_err());
        assert!(absolute_components(Path::new(r"\root-relative")).is_err());
        let (absolute, _, parts) = absolute_components(Path::new(r"relative\subdir")).unwrap();
        assert!(absolute.is_absolute());
        assert!(parts.ends_with(&[OsString::from("relative"), OsString::from("subdir")]));
    }

    #[test]
    fn parses_drive_and_unc_roots() {
        let (_, drive, drive_parts) =
            absolute_components(Path::new(r"C:\game\manosaba.app")).unwrap();
        assert_eq!(drive, Path::new(r"C:\"));
        assert_eq!(
            drive_parts,
            [OsString::from("game"), OsString::from("manosaba.app")]
        );

        let (_, unc, unc_parts) =
            absolute_components(Path::new(r"\\server\share\manosaba.app")).unwrap();
        assert_eq!(unc, Path::new(r"\\server\share\"));
        assert_eq!(unc_parts, [OsString::from("manosaba.app")]);
    }

    #[test]
    fn rejects_unc_output_before_opening_the_share() {
        let temp = tempfile::tempdir().unwrap();
        let app_path = temp.path().join("game.app");
        fs::create_dir(&app_path).unwrap();
        let app = ValidatedAppPath::new(&app_path).unwrap();
        let error =
            ValidatedOutputPath::new(Path::new(r"\\server\share\dataset"), &app).unwrap_err();
        assert!(error.to_string().contains("network output"));
    }

    #[test]
    fn rejects_directory_symlink_when_supported() {
        let temp = tempfile::tempdir().unwrap();
        let app_path = temp.path().join("game.app");
        fs::create_dir(&app_path).unwrap();
        let link = temp.path().join("alias");
        if std::os::windows::fs::symlink_dir(&app_path, &link).is_ok() {
            assert!(ValidatedAppPath::new(&link).is_err());
        }
    }

    #[test]
    fn rejects_junction_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let app_path = temp.path().join("game.app");
        fs::create_dir(&app_path).unwrap();
        let junction = temp.path().join("junction");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&app_path)
            .status()
            .unwrap();
        assert!(status.success(), "could not create junction fixture");
        assert!(ValidatedAppPath::new(&junction).is_err());
    }
}
