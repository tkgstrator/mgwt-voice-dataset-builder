use mgwt_voice_dataset_builder::paths::{
    ValidatedAppPath, ValidatedOutputPath, validate_separation,
};
use std::{fs, os::unix::fs::symlink, path::Path};

fn app(root: &Path) -> std::path::PathBuf {
    let path = root.join("game.app");
    fs::create_dir(&path).unwrap();
    path
}

#[test]
fn output_must_be_separate_from_app_in_both_directions() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let app = app(&root);
    for output in [app.clone(), app.join("inside")] {
        assert!(validate_separation(&app, &output).is_err(), "{output:?}");
    }
    assert!(validate_separation(&app, &root).is_err());
    assert!(validate_separation(&app, &root.join("dataset")).is_ok());
}

#[test]
fn existing_output_leaf_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    fs::create_dir(root.join("dataset")).unwrap();
    assert!(ValidatedOutputPath::new(&root.join("dataset")).is_err());
}

#[test]
fn symlinked_ancestors_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let real = root.join("real");
    fs::create_dir(&real).unwrap();
    symlink(&real, root.join("link")).unwrap();
    assert!(ValidatedOutputPath::new(&root.join("link/dataset")).is_err());
    assert!(ValidatedAppPath::new(&root.join("link")).is_err());
}

#[test]
fn fresh_output_binds_parent_and_preserves_literal_leaf() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let output = ValidatedOutputPath::new(&root.join("dataset")).unwrap();
    assert_eq!(output.leaf().to_str(), Some("dataset"));
    assert_eq!(
        rustix::fs::fstat(output.parent()).unwrap().st_mode & 0o170000,
        0o040000
    );
}

#[test]
fn rustix_exclusive_directory_rename_preserves_an_existing_destination() {
    use rustix::fs::{Mode, OFlags, RenameFlags, openat, renameat_with};
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    fs::create_dir(root.join("source")).unwrap();
    fs::create_dir(root.join("occupied")).unwrap();
    fs::write(root.join("occupied/marker"), b"keep").unwrap();
    let parent = openat(
        rustix::fs::CWD,
        &root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .unwrap();
    assert!(
        renameat_with(
            &parent,
            "source",
            &parent,
            "occupied",
            RenameFlags::NOREPLACE
        )
        .is_err()
    );
    assert!(root.join("source").is_dir());
    assert_eq!(fs::read(root.join("occupied/marker")).unwrap(), b"keep");
    renameat_with(
        &parent,
        "source",
        &parent,
        "promoted",
        RenameFlags::NOREPLACE,
    )
    .unwrap();
    assert!(!root.join("source").exists());
    assert!(root.join("promoted").is_dir());
}
