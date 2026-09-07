//! Unit tests for session directory layout.

use super::*;

#[test]
fn ensure_private_dir_rejects_leaf_symlink_without_chmodding_target() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    let link = root.path().join("link");
    symlink(&target, &link).unwrap();

    let error = ensure_private_dir(&link).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("without following symbolic links"),
        "{error:#}"
    );
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn ensure_private_dir_rejects_symlink_ancestor_without_creating_beneath_it() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    fs::create_dir(&target).unwrap();
    let link = root.path().join("link");
    symlink(&target, &link).unwrap();

    assert!(ensure_private_dir(&link.join("child")).is_err());
    assert!(!target.join("child").exists());
}

#[test]
fn ensure_private_dir_validates_type_before_chmod() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("ordinary-file");
    fs::write(&file, b"not a directory").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();

    assert!(ensure_private_dir(&file).is_err());
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[test]
fn ensure_private_dir_chmods_verified_directory() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("private");
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();

    ensure_private_dir(&directory).unwrap();
    assert_eq!(
        fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[test]
fn explicit_relative_path_overrides_are_resolved_once() {
    let resolved = absolute_override_path(PathBuf::from("state"), "APLEXER_STATE_DIR").unwrap();
    assert!(resolved.is_absolute());
    assert_eq!(resolved, env::current_dir().unwrap().join("state"));
}

#[test]
fn xdg_paths_must_be_absolute() {
    let error = absolute_xdg_path(PathBuf::from("runtime"), "XDG_RUNTIME_DIR").unwrap_err();
    assert!(error.to_string().contains("must be an absolute path"));
    assert_eq!(
        absolute_xdg_path(PathBuf::from("/run/user/1000"), "XDG_RUNTIME_DIR").unwrap(),
        PathBuf::from("/run/user/1000")
    );
}
