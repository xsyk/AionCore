use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

fn mode(path: &Path, value: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(value)).unwrap();
}

struct Fixture {
    root: tempfile::TempDir,
    path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        assert_ne!(unsafe { libc::geteuid() }, 0, "run QA tests as the service account");
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("etc/aioneasiful/qa-session-launch/driver/launch_qa.py");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"trusted contents").unwrap();
        mode(&path, 0o555);
        Self { root, path }
    }

    fn files(&self) -> TrustedFiles {
        TrustedFiles::for_test(self.root.path().to_path_buf(), unsafe { libc::geteuid() })
    }

    fn directories(&self) -> Vec<PathBuf> {
        ["", "etc", "etc/aioneasiful", "etc/aioneasiful/qa-session-launch", "etc/aioneasiful/qa-session-launch/driver"]
            .into_iter().map(|path| self.root.path().join(path)).collect()
    }
}

fn code<T>(result: Result<T>, expected: &str) {
    match result {
        Err(error) => assert_eq!(error.0, expected),
        Ok(_) => panic!("expected {expected}"),
    }
}

#[test]
fn qa_session_launch_admin_files_are_readable_and_executable_without_service_write() {
    let fixture = Fixture::new();
    assert_eq!(fixture.files().read_admin(&fixture.path, 128).unwrap(), b"trusted contents");
    fixture.files().check_driver(&fixture.path).unwrap();
    mode(&fixture.path, 0o444);
    assert_eq!(fixture.files().read_admin(&fixture.path, 128).unwrap(), b"trusted contents");
    code(fixture.files().check_driver(&fixture.path), "QA_TRUSTED_PATH_REJECTED");
}

#[test]
fn qa_session_launch_every_parent_and_anchor_reject_symlinks() {
    let fixture = Fixture::new();
    for directory in fixture.directories().into_iter().skip(1) {
        let saved = directory.with_extension("saved");
        std::fs::rename(&directory, &saved).unwrap();
        symlink(&saved, &directory).unwrap();
        code(fixture.files().read_admin(&fixture.path, 128), "QA_TRUSTED_PATH_REJECTED");
        std::fs::remove_file(&directory).unwrap();
        std::fs::rename(saved, directory).unwrap();
    }
    let alias_root = tempfile::tempdir().unwrap();
    let alias = alias_root.path().join("alias");
    symlink(fixture.root.path(), &alias).unwrap();
    let files = TrustedFiles::for_test(alias.clone(), unsafe { libc::geteuid() });
    let path = alias.join(fixture.path.strip_prefix(fixture.root.path()).unwrap());
    code(files.read_admin(&path, 128), "QA_TRUSTED_PATH_REJECTED");
}

#[test]
fn qa_session_launch_every_parent_rejects_group_or_world_write() {
    let fixture = Fixture::new();
    for directory in fixture.directories() {
        for bad_mode in [0o775, 0o757, 0o777] {
            mode(&directory, bad_mode);
            code(fixture.files().read_admin(&fixture.path, 128), "QA_TRUSTED_PATH_REJECTED");
        }
        mode(&directory, 0o755);
    }
}

#[test]
fn qa_session_launch_untrusted_uid_is_rejected_without_chown() {
    let fixture = Fixture::new();
    let uid = unsafe { libc::geteuid() };
    let files = TrustedFiles::for_test(fixture.root.path().to_path_buf(), uid + 1);
    code(files.read_admin(&fixture.path, 128), "QA_TRUSTED_PATH_REJECTED");
    code(regular_file(&std::fs::metadata(&fixture.path).unwrap(), uid + 1, false), "QA_TRUSTED_PATH_REJECTED");
    // All parents have the allowed admin UID, while this final executable's UID
    // differs from the injected service UID: exercise final-file owner checking.
    let files = TrustedFiles { anchor: fixture.root.path().to_path_buf(), admin_uid: uid, service_uid: uid + 1, opens: Default::default() };
    for directory in fixture.directories() { mode(&directory, 0o555); }
    code(files.check_binary(&fixture.path), "QA_TRUSTED_PATH_REJECTED");
    for directory in fixture.directories() { mode(&directory, 0o755); }
}

#[test]
fn qa_session_launch_effective_service_write_is_rejected() {
    let fixture = Fixture::new();
    let uid = unsafe { libc::geteuid() };
    let files = TrustedFiles { anchor: fixture.root.path().to_path_buf(), admin_uid: uid, service_uid: uid + 1, opens: Default::default() };
    code(files.read_admin(&fixture.path, 128), "QA_TRUSTED_PATH_REJECTED");
    for directory in fixture.directories() { mode(&directory, 0o555); }
    assert_eq!(files.read_admin(&fixture.path, 128).unwrap(), b"trusted contents");
    mode(&fixture.path, 0o755);
    code(files.read_admin(&fixture.path, 128), "QA_TRUSTED_PATH_REJECTED");
    for directory in fixture.directories() { mode(&directory, 0o755); }
}

#[test]
fn qa_session_launch_final_links_and_hardlinks_are_rejected() {
    let fixture = Fixture::new();
    let hardlink = fixture.path.with_extension("hardlink");
    std::fs::hard_link(&fixture.path, &hardlink).unwrap();
    code(fixture.files().read_admin(&fixture.path, 128), "QA_TRUSTED_PATH_REJECTED");
    std::fs::remove_file(&hardlink).unwrap();
    let saved = fixture.path.with_extension("saved");
    std::fs::rename(&fixture.path, &saved).unwrap();
    symlink(saved, &fixture.path).unwrap();
    code(fixture.files().read_admin(&fixture.path, 128), "QA_TRUSTED_PATH_REJECTED");
}

#[test]
fn qa_session_launch_file_modes_special_bits_and_unreadability_are_rejected() {
    let fixture = Fixture::new();
    for bad_mode in [0o666, 0o557, 0o577, 0o4555, 0o2555, 0o1555, 0o000] {
        mode(&fixture.path, bad_mode);
        code(fixture.files().read_admin(&fixture.path, 128), "QA_TRUSTED_PATH_REJECTED");
    }
    mode(&fixture.path, 0o555);
}

#[test]
fn qa_session_launch_reads_are_bounded_and_nonregular_files_never_block() {
    let fixture = Fixture::new();
    code(fixture.files().read_admin(&fixture.path, 2), "QA_FILE_SIZE_REJECTED");
    mode(&fixture.path, 0o600);
    std::fs::write(&fixture.path, b"").unwrap();
    mode(&fixture.path, 0o444);
    code(fixture.files().read_admin(&fixture.path, 128), "QA_FILE_SIZE_REJECTED");
    std::fs::remove_file(&fixture.path).unwrap();
    std::fs::create_dir(&fixture.path).unwrap();
    code(fixture.files().read_admin(&fixture.path, 128), "QA_TRUSTED_PATH_REJECTED");
    std::fs::remove_dir(&fixture.path).unwrap();
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(fixture.path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o444) }, 0);
    code(fixture.files().read_admin(&fixture.path, 128), "QA_TRUSTED_PATH_REJECTED");
}

#[test]
fn qa_session_launch_changed_file_metadata_is_rejected_after_open() {
    let fixture = Fixture::new();
    let checked = fixture.files().open(&fixture.path, false, false).unwrap();
    mode(&fixture.path, 0o600);
    std::fs::write(&fixture.path, b"changed bytes and size").unwrap();
    mode(&fixture.path, 0o555);
    code(checked.finish(), "QA_FILE_CHANGED");
}

#[test]
fn qa_session_launch_changed_parent_is_rejected_after_open() {
    let fixture = Fixture::new();
    let parent = File::open(fixture.path.parent().unwrap()).unwrap();
    // A new sibling can share the directory's timestamp tick and allocation size.
    // Seed an old mtime so the write below must produce an observable change.
    parent.set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH)).unwrap();
    assert_eq!(parent.metadata().unwrap().modified().unwrap(), std::time::UNIX_EPOCH);
    let checked = fixture.files().open(&fixture.path, false, false).unwrap();
    std::fs::write(fixture.path.with_extension("replacement"), b"admin-side replacement").unwrap();
    assert_ne!(parent.metadata().unwrap().modified().unwrap(), std::time::UNIX_EPOCH);
    code(checked.finish(), "QA_FILE_CHANGED");
}

#[test]
fn qa_session_launch_binary_rejects_linked_paths_and_accepts_private_service_uid() {
    let fixture = Fixture::new();
    mode(&fixture.path, 0o600);
    std::fs::write(&fixture.path, b"\x7fELFtest-only").unwrap();
    mode(&fixture.path, 0o700);
    fixture.files().check_binary(&fixture.path).unwrap();
    let directory = fixture.path.parent().unwrap();
    let saved = directory.with_extension("saved");
    std::fs::rename(directory, &saved).unwrap();
    symlink(saved, directory).unwrap();
    code(fixture.files().check_binary(&fixture.path), "QA_TRUSTED_PATH_REJECTED");
}
