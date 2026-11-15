//! Linux directory-FD traversal. Root-owned ancestors cannot be replaced by the
//! service account; every open rejects links, including the final component.

use std::ffi::CString;
use std::fs::{File, Metadata};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use super::QaLaunchError;

type Result<T> = std::result::Result<T, QaLaunchError>;

pub(super) struct TrustedFiles {
    anchor: PathBuf,
    admin_uid: u32,
    service_uid: u32,
    #[cfg(test)]
    opens: std::sync::atomic::AtomicUsize,
}

pub(super) struct CheckedFile {
    file: File,
    before: Metadata,
    parents: Vec<(File, Metadata)>,
}

fn rejected() -> QaLaunchError {
    QaLaunchError("QA_TRUSTED_PATH_REJECTED")
}

fn stable(before: &Metadata, after: &Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.mode() == after.mode()
        && before.uid() == after.uid()
        && before.gid() == after.gid()
        && before.nlink() == after.nlink()
        && before.size() == after.size()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

fn regular_file(metadata: &Metadata, expected_uid: u32, service_binary: bool) -> Result<()> {
    let forbidden_mode = if service_binary { 0o077 } else { 0o022 };
    if !metadata.is_file()
        || metadata.uid() != expected_uid
        || metadata.nlink() != 1
        || metadata.mode() & forbidden_mode != 0
        || metadata.mode() & 0o7000 != 0
    {
        return Err(rejected());
    }
    Ok(())
}

fn open_at(parent: &File, name: &std::ffi::OsStr, directory: bool) -> Result<File> {
    use std::os::unix::ffi::OsStrExt;
    let name = CString::new(name.as_bytes()).map_err(|_| rejected())?;
    let mut flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    if directory {
        flags |= libc::O_DIRECTORY;
    }
    // SAFETY: the CString and borrowed directory descriptor live through openat;
    // a successful descriptor is immediately transferred to exactly one File.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(rejected());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn executable_at(parent: &File, name: &std::ffi::OsStr) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let name = CString::new(name.as_bytes()).map_err(|_| rejected())?;
    // SAFETY: both arguments are valid for this call. Effective credentials are
    // required: access() would check the wrong identity in a set-id process.
    if unsafe {
        libc::faccessat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::R_OK | libc::X_OK,
            libc::AT_EACCESS | libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(rejected());
    }
    Ok(())
}

impl TrustedFiles {
    pub(super) fn production() -> Result<Self> {
        let service_uid = unsafe { libc::geteuid() };
        // Root can always write root-owned authority. A diagnostic Core must run
        // as the unprivileged service identity, not as the administrator.
        if service_uid == 0 || unsafe { libc::getuid() } != service_uid {
            return Err(rejected());
        }
        Ok(Self {
            anchor: PathBuf::from("/"),
            admin_uid: 0,
            service_uid,
            #[cfg(test)]
            opens: Default::default(),
        })
    }

    #[cfg(test)]
    pub(super) fn for_test(anchor: PathBuf, admin_uid: u32) -> Self {
        Self {
            anchor,
            admin_uid,
            service_uid: unsafe { libc::geteuid() },
            opens: Default::default(),
        }
    }

    #[cfg(test)]
    pub(super) fn open_count(&self) -> usize {
        self.opens.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn admin_not_writable(&self, parent: &File, name: &std::ffi::OsStr) -> Result<()> {
        // Unit fixtures substitute their own UID for root. Their ownership role
        // is injected only in cfg(test); production always checks real access.
        #[cfg(test)]
        if self.admin_uid == self.service_uid {
            return Ok(());
        }
        use std::os::unix::ffi::OsStrExt;
        let name = CString::new(name.as_bytes()).map_err(|_| rejected())?;
        let access = unsafe {
            libc::faccessat(
                parent.as_raw_fd(), name.as_ptr(), libc::W_OK,
                libc::AT_EACCESS | libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if access == 0 {
            return Err(rejected());
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EACCES) | Some(libc::EROFS) => Ok(()),
            _ => Err(rejected()),
        }
    }

    fn directory(&self, file: &File, service_path: bool) -> Result<Metadata> {
        let metadata = file.metadata().map_err(|_| rejected())?;
        let owner_ok = metadata.uid() == self.admin_uid
            || (service_path && metadata.uid() == self.service_uid);
        if !metadata.is_dir() || !owner_ok || metadata.mode() & 0o022 != 0 {
            return Err(rejected());
        }
        executable_at(file, std::ffi::OsStr::new("."))?;
        if !service_path {
            self.admin_not_writable(file, std::ffi::OsStr::new("."))?;
        }
        Ok(metadata)
    }

    fn open(&self, path: &Path, service_binary: bool, executable: bool) -> Result<CheckedFile> {
        #[cfg(test)]
        self.opens.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        use std::os::unix::fs::OpenOptionsExt;
        let relative = path.strip_prefix(&self.anchor).map_err(|_| rejected())?;
        let components: Vec<_> = relative.components().collect();
        if components.is_empty() || components.iter().any(|part| !matches!(part, Component::Normal(_))) {
            return Err(rejected());
        }
        let mut parent = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_NONBLOCK)
            .open(&self.anchor)
            .map_err(|_| rejected())?;
        let mut parents = Vec::new();
        let mut before_parent = self.directory(&parent, service_binary)?;
        for part in &components[..components.len() - 1] {
            let next = open_at(&parent, part.as_os_str(), true)?;
            parents.push((parent, before_parent));
            parent = next;
            before_parent = self.directory(&parent, service_binary)?;
        }
        let name = components.last().ok_or_else(rejected)?.as_os_str();
        let file = open_at(&parent, name, false)?;
        let before = file.metadata().map_err(|_| rejected())?;
        let expected_uid = if service_binary { self.service_uid } else { self.admin_uid };
        regular_file(&before, expected_uid, service_binary)?;
        if executable {
            executable_at(&parent, name)?;
        }
        if !service_binary {
            self.admin_not_writable(&parent, name)?;
        }
        // A root-side replacement between openat and faccessat is also refused.
        let current = open_at(&parent, name, false)?;
        if !stable(&before, &current.metadata().map_err(|_| rejected())?) {
            return Err(rejected());
        }
        parents.push((parent, before_parent));
        Ok(CheckedFile { file, before, parents })
    }

    pub(super) fn read_admin(&self, path: &Path, limit: usize) -> Result<Vec<u8>> {
        self.open(path, false, false)?.read_bounded(limit)
    }

    pub(super) fn check_driver(&self, path: &Path) -> Result<()> {
        // Importing the probe needs read access; both installed driver files
        // intentionally have executable permissions for deployment preflight.
        self.open(path, false, true)?.finish()
    }

    pub(super) fn check_binary(&self, path: &Path) -> Result<()> {
        let mut checked = self.open(path, true, true)?;
        let mut magic = [0; 4];
        checked.file.read_exact(&mut magic).map_err(|_| rejected())?;
        if magic != *b"\x7fELF" {
            return Err(rejected());
        }
        checked.finish()
    }
}

impl CheckedFile {
    fn read_bounded(mut self, limit: usize) -> Result<Vec<u8>> {
        if self.before.len() == 0 || self.before.len() > limit as u64 {
            return Err(QaLaunchError("QA_FILE_SIZE_REJECTED"));
        }
        let mut bytes = Vec::with_capacity(self.before.len() as usize);
        (&mut self.file)
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| rejected())?;
        if bytes.len() > limit || bytes.len() as u64 != self.before.len() {
            return Err(QaLaunchError("QA_FILE_SIZE_REJECTED"));
        }
        self.finish()?;
        Ok(bytes)
    }

    fn finish(self) -> Result<()> {
        if !stable(&self.before, &self.file.metadata().map_err(|_| rejected())?) {
            return Err(QaLaunchError("QA_FILE_CHANGED"));
        }
        for (parent, before) in self.parents {
            if !stable(&before, &parent.metadata().map_err(|_| rejected())?) {
                return Err(QaLaunchError("QA_FILE_CHANGED"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "files_tests.rs"]
mod tests;
