//! Streaming one upload into a project directory (`POST /api/fs/upload-to-project`).
//!
//! The route resolves the pe-addressed target (ownership + lexical containment via
//! `resolve_reference`) and feeds the multipart body through [`IProjectUpload`].
//! Everything that touches the disk lives here:
//!
//! - The target directory is checked on disk: an existing directory whose real
//!   path stays inside the folder root's.
//! - The content streams into a private temp file (`.aionui-upload-<random>`,
//!   hidden from the Explorer tree), so the final name never holds a partial file.
//! - `finish` claims the first free name by hard-linking the temp to it. Linking
//!   to a taken name fails, so nothing is ever overwritten and two uploads never
//!   claim the same name. Where hard links are unavailable (FAT volumes, some
//!   network mounts) the name is claimed with an empty `create_new` placeholder
//!   instead, and the temp is renamed over it.
//! - A guard removes the temp on every path — success, error, rejected request,
//!   dropped request future — and clears the root's `/api/fs/list` cache entry,
//!   which may have picked the temp up. Temps a crash left behind are swept from
//!   the target directory once they are a day old.

use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use aionui_common::constants::PROJECT_UPLOAD_TEMP_PREFIX;
use tokio::io::{AsyncWriteExt, BufWriter};

use super::{ListingCache, candidate_name, split_ext, validate_new_dir_name};
use crate::error::FileError;
use crate::traits::IProjectUpload;
use crate::types::UploadedFile;

/// Longest accepted file name or path segment, in UTF-8 bytes — the usual
/// filesystem limit for one path component (the same bound folder creation uses).
const MAX_FILE_NAME_BYTES: usize = 255;

/// Longest accepted target path, in bytes (Linux `PATH_MAX`).
const MAX_PATH_BYTES: usize = 4096;

/// Candidate names tried before giving up (the drag-import copy uses the same cap).
const MAX_NAME_ATTEMPTS: usize = 10_000;

/// Buffer between the incoming body chunks and the file, so small chunks do not
/// each cost a blocking-pool round trip. Bounds an upload's memory.
const WRITE_BUFFER_BYTES: usize = 256 * 1024;

/// Age past which a temp in the target directory is a crash leftover.
const STALE_TEMP_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Random hex digits in a temp name.
const TEMP_ID_LEN: usize = 16;

/// The local-disk [`IProjectUpload`].
pub(super) struct LocalProjectUpload {
    // Declared before `temp` so the file handle is released before the guard
    // removes the path.
    writer: BufWriter<tokio::fs::File>,
    temp: TempFile,
    /// Real path of the target directory.
    dir: PathBuf,
    /// The validated requested name.
    requested: String,
    written: u64,
    limit: u64,
}

impl LocalProjectUpload {
    /// Check `dir`, sweep stale temps from it and open a fresh temp there.
    ///
    /// `root` is the folder root of the entry `dir` was resolved from; `limit`
    /// caps the content in bytes; `listing` is the `/api/fs/list` cache to clear
    /// once the temp is gone.
    pub(super) async fn begin(
        root: &Path,
        dir: &Path,
        file_name: &str,
        limit: u64,
        listing: ListingCache,
    ) -> Result<Self, FileError> {
        let requested = validate_file_name(file_name)?.to_owned();
        let (root, dir) = (root.to_path_buf(), dir.to_path_buf());
        // The guard travels inside the task's output, so even if this future is
        // dropped while the task runs, the temp it creates is removed with it.
        let (file, temp, dir) = tokio::task::spawn_blocking(move || {
            let (root_real, dir_real) = upload_dir_sync(&root, &dir)?;
            sweep_stale_temps(&dir_real, STALE_TEMP_AGE);
            let listing = ListingEntry {
                cache: listing,
                key: root_real.to_string_lossy().into_owned(),
            };
            let (file, temp) = create_temp_sync(&dir_real, listing)?;
            Ok::<_, FileError>((file, temp, dir_real))
        })
        .await
        .map_err(|e| FileError::Internal(format!("upload task failed: {e}")))??;

        Ok(Self {
            writer: BufWriter::with_capacity(WRITE_BUFFER_BYTES, tokio::fs::File::from_std(file)),
            temp,
            dir,
            requested,
            written: 0,
            limit,
        })
    }
}

#[async_trait::async_trait]
impl IProjectUpload for LocalProjectUpload {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), FileError> {
        let len = chunk.len() as u64;
        if self.written.saturating_add(len) > self.limit {
            return Err(FileError::PayloadTooLarge(format!(
                "file exceeds the upload limit of {} bytes",
                self.limit
            )));
        }
        self.writer
            .write_all(chunk)
            .await
            .map_err(|e| io_failure(&e, "cannot write uploaded file"))?;
        self.written += len;
        Ok(())
    }

    async fn finish(self: Box<Self>) -> Result<UploadedFile, FileError> {
        let mut upload = *self;
        // `BufWriter::flush` also waits for the file's in-flight write, so write
        // errors surface here rather than being lost when the handle drops.
        upload
            .writer
            .flush()
            .await
            .map_err(|e| io_failure(&e, "cannot write uploaded file"))?;
        let LocalProjectUpload {
            writer,
            temp,
            dir,
            requested,
            written,
            ..
        } = upload;
        drop(writer);

        let (name, renamed) = tokio::task::spawn_blocking(move || {
            let claimed = claim_name_sync(&dir, temp.path(), &requested, MAX_NAME_ATTEMPTS, true);
            // A hard link left the temp as a second name of the file, a rename
            // moved it away, a failure left it alone: dropping the guard removes
            // whatever is left and clears the listing either way.
            drop(temp);
            claimed.map(|name| {
                let renamed = name != requested;
                (name, renamed)
            })
        })
        .await
        .map_err(|e| FileError::Internal(format!("upload task failed: {e}")))??;

        Ok(UploadedFile {
            name,
            size: written,
            renamed,
        })
    }
}

/// One root's entry in the `/api/fs/list` cache.
struct ListingEntry {
    cache: ListingCache,
    key: String,
}

/// The private file an upload streams into. Dropping it removes the file if it
/// is still there and clears the root's listing entry, which may have picked the
/// temp up (`/api/fs/list` includes dotfiles).
struct TempFile {
    path: PathBuf,
    listing: ListingEntry,
}

impl TempFile {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != ErrorKind::NotFound
        {
            tracing::warn!(path = %self.path.display(), error = %e, "fs: could not remove an upload temp file");
        }
        self.listing.cache.remove(&self.listing.key);
    }
}

/// An upload name must be one plain path component the platform can store: the
/// folder-name rule (no separator or control character, not `.` or `..`) plus,
/// on Windows, what Windows refuses.
fn validate_file_name(raw: &str) -> Result<&str, FileError> {
    if raw.trim().len() > MAX_FILE_NAME_BYTES {
        return Err(FileError::BadRequest("file name is too long".to_owned()));
    }
    let name = validate_new_dir_name(raw).map_err(|_| invalid_file_name())?;
    if cfg!(windows) && !windows_can_store(name) {
        return Err(invalid_file_name());
    }
    Ok(name)
}

fn invalid_file_name() -> FileError {
    FileError::BadRequest("invalid file name".to_owned())
}

/// Whether Windows can store `name`: none of `< > : " | ? *`, no trailing dot or
/// space, and not a reserved device name (`CON`, `nul.txt`, `Com1`, …) — Windows
/// reserves those with any extension and in any case.
fn windows_can_store(name: &str) -> bool {
    if name.contains(['<', '>', ':', '"', '|', '?', '*']) || name.ends_with(['.', ' ']) {
        return false;
    }
    let device = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    let numbered = |prefix: &str| {
        device.strip_prefix(prefix).is_some_and(|n| {
            matches!(
                n,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "\u{b9}" | "\u{b2}" | "\u{b3}"
            )
        })
    };
    !(matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL") || numbered("COM") || numbered("LPT"))
}

/// Resolve the upload directory to its real path: it must be an existing
/// directory that stays inside the folder root's real path. `resolve_reference`
/// checks containment lexically only, so without this a symlink inside the project
/// could aim the write anywhere on the host — the same access-time guard the
/// Explorer's own create/copy commands apply. Returns `(root_real, dir_real)`.
///
/// Messages are path-free: the client addressed the directory by identity.
fn upload_dir_sync(root: &Path, dir: &Path) -> Result<(PathBuf, PathBuf), FileError> {
    check_target_path(dir)?;
    let root_real = std::fs::canonicalize(root).map_err(|e| upload_dir_error(&e))?;
    let dir_real = std::fs::canonicalize(dir).map_err(|e| upload_dir_error(&e))?;
    if !dir_real.starts_with(&root_real) {
        return Err(FileError::BadRequest(
            "upload target is outside the project folder".to_owned(),
        ));
    }
    let metadata = std::fs::metadata(&dir_real).map_err(|e| upload_dir_error(&e))?;
    if !metadata.is_dir() {
        return Err(FileError::BadRequest("upload target is not a directory".to_owned()));
    }
    Ok((root_real, dir_real))
}

/// Refuse target paths no filesystem takes before asking one: a NUL, a segment
/// over 255 bytes, or more than 4096 bytes in all.
fn check_target_path(dir: &Path) -> Result<(), FileError> {
    if dir.to_string_lossy().contains('\0') {
        return Err(FileError::BadRequest("invalid upload target path".to_owned()));
    }
    let too_long = dir.as_os_str().len() > MAX_PATH_BYTES
        || dir
            .components()
            .any(|c| matches!(c, Component::Normal(segment) if segment.len() > MAX_FILE_NAME_BYTES));
    if too_long {
        return Err(FileError::BadRequest("upload target path is too long".to_owned()));
    }
    Ok(())
}

/// Classify a failure to resolve the target directory: what the client named is
/// wrong (400), not allowed (403), or the disk failed (500).
fn upload_dir_error(e: &io::Error) -> FileError {
    match e.kind() {
        ErrorKind::NotFound => FileError::BadRequest("upload target directory does not exist".to_owned()),
        // A path component is a file (`a.txt/sub`).
        ErrorKind::NotADirectory => FileError::BadRequest("upload target is not a directory".to_owned()),
        ErrorKind::InvalidInput => FileError::BadRequest("invalid upload target path".to_owned()),
        ErrorKind::InvalidFilename => FileError::BadRequest("upload target path is too long".to_owned()),
        _ if is_symlink_loop(e) => FileError::BadRequest("upload target path has a symlink loop".to_owned()),
        _ => io_failure(e, "cannot resolve upload target"),
    }
}

/// `ErrorKind::FilesystemLoop` is not stable, so the loop is told by its errno.
#[cfg(unix)]
fn is_symlink_loop(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::ELOOP)
}

#[cfg(not(unix))]
fn is_symlink_loop(_: &io::Error) -> bool {
    false
}

/// An IO failure the client did not cause: missing permission or a read-only
/// filesystem is 403 (as folder creation reports it), anything else a 500.
fn io_failure(e: &io::Error, context: &str) -> FileError {
    match e.kind() {
        ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem => {
            FileError::Forbidden("permission denied".to_owned())
        }
        _ => FileError::Internal(format!("{context}: {e}")),
    }
}

/// Remove temps older than `max_age` that a crashed upload left in `dir`.
/// Best effort: nothing here may fail the upload that triggered it.
fn sweep_stale_temps(dir: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(PROJECT_UPLOAD_TEMP_PREFIX)
        {
            continue;
        }
        // `DirEntry::metadata` does not follow symlinks: only plain files go.
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let stale = metadata.is_file()
            && metadata
                .modified()
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > max_age);
        if !stale {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => tracing::info!(path = %entry.path().display(), "fs: removed a stale upload temp file"),
            Err(e) => {
                tracing::warn!(path = %entry.path().display(), error = %e, "fs: could not remove a stale upload temp file")
            }
        }
    }
}

/// Create the upload's private temp in `dir`. `create_new` guarantees it is a
/// fresh file, never an existing entry.
fn create_temp_sync(dir: &Path, listing: ListingEntry) -> Result<(std::fs::File, TempFile), FileError> {
    // A 64-bit random id practically never collides; a few retries cover it.
    for _ in 0..8 {
        let id = aionui_common::generate_id_with_length(Some(TEMP_ID_LEN));
        let path = dir.join(format!("{PROJECT_UPLOAD_TEMP_PREFIX}{id}"));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((file, TempFile { path, listing })),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_failure(&e, "cannot create upload temp file")),
        }
    }
    Err(FileError::Internal("cannot pick a free upload temp name".to_owned()))
}

/// Give the finished `temp` its final name in `dir` and return that name: the
/// first free candidate for `requested`. `link` selects the strategy; it falls
/// back to the placeholder by itself when the filesystem refuses hard links.
fn claim_name_sync(
    dir: &Path,
    temp: &Path,
    requested: &str,
    max_attempts: usize,
    mut link: bool,
) -> Result<String, FileError> {
    for attempt in 0..max_attempts {
        let name = fitted_candidate(requested, attempt);
        let path = dir.join(&name);
        if link {
            match std::fs::hard_link(temp, &path) {
                Ok(()) => return Ok(name),
                Err(e) if name_taken(&e, &path) => continue,
                Err(e) => {
                    // No hard links on this filesystem: claim names with an empty
                    // placeholder from here on (retrying this candidate).
                    tracing::debug!(error = %e, "fs: hard link unavailable for an upload, using a placeholder");
                    link = false;
                }
            }
        }
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(placeholder) => {
                drop(placeholder);
                // Renaming over our own placeholder replaces it atomically.
                return match std::fs::rename(temp, &path) {
                    Ok(()) => Ok(name),
                    Err(e) => {
                        let _ = std::fs::remove_file(&path);
                        Err(claim_error(&e))
                    }
                };
            }
            Err(e) if name_taken(&e, &path) => {}
            Err(e) => return Err(claim_error(&e)),
        }
    }
    Err(FileError::Conflict(
        "too many files with this name in the upload target".to_owned(),
    ))
}

/// Whether a failed claim of `path` means the name is already taken (by a file,
/// a directory or a symlink, dangling or not — none of them is followed).
fn name_taken(e: &io::Error, path: &Path) -> bool {
    // A directory holding the name may surface as access denied rather than
    // `AlreadyExists` on some platforms; it is taken either way.
    e.kind() == ErrorKind::AlreadyExists || (e.kind() == ErrorKind::PermissionDenied && path.is_dir())
}

fn claim_error(e: &io::Error) -> FileError {
    match e.kind() {
        // The filesystem allows shorter names than the 255 bytes checked up front.
        ErrorKind::InvalidFilename => FileError::BadRequest("file name is too long".to_owned()),
        _ => io_failure(e, "cannot store uploaded file"),
    }
}

/// [`candidate_name`] for a file, with the stem shortened (at a character
/// boundary) when the ` copy N` suffix would push the name past 255 bytes. An
/// extension too long to keep beside the suffix is shortened as part of the stem.
fn fitted_candidate(requested: &str, attempt: usize) -> String {
    let candidate = candidate_name(requested, attempt, false);
    if candidate.len() <= MAX_FILE_NAME_BYTES {
        return candidate;
    }
    let suffix = if attempt == 1 {
        " copy".to_owned()
    } else {
        format!(" copy {attempt}")
    };
    let (stem, ext) = split_ext(requested);
    let (stem, ext) = if suffix.len() + ext.len() < MAX_FILE_NAME_BYTES {
        (stem, ext)
    } else {
        (requested, "")
    };
    let mut end = (MAX_FILE_NAME_BYTES - suffix.len() - ext.len()).min(stem.len());
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}{ext}", &stem[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Arc;
    use std::time::SystemTime;

    use dashmap::DashMap;

    const LIMIT: u64 = 1024;

    async fn begin(root: &Path, dir: &Path, name: &str) -> Result<LocalProjectUpload, FileError> {
        LocalProjectUpload::begin(root, dir, name, LIMIT, Arc::new(DashMap::new())).await
    }

    async fn finish(upload: LocalProjectUpload) -> Result<UploadedFile, FileError> {
        Box::new(upload).finish().await
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn temps(dir: &Path) -> Vec<String> {
        names(dir)
            .into_iter()
            .filter(|name| name.starts_with(PROJECT_UPLOAD_TEMP_PREFIX))
            .collect()
    }

    fn bad_request_message(result: Result<LocalProjectUpload, FileError>) -> String {
        match result {
            Err(FileError::BadRequest(message)) => message,
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got an upload"),
        }
    }

    #[tokio::test]
    async fn content_streams_into_a_hidden_temp_until_finish_names_it() {
        let root = tempfile::tempdir().unwrap();
        let mut upload = begin(root.path(), root.path(), "a.txt").await.unwrap();
        upload.write_chunk(b"hel").await.unwrap();
        upload.write_chunk(b"lo").await.unwrap();

        assert!(
            !root.path().join("a.txt").exists(),
            "the final name holds no partial file"
        );
        assert_eq!(temps(root.path()).len(), 1, "{:?}", names(root.path()));
        let temp = &temps(root.path())[0];
        assert_eq!(temp.len(), PROJECT_UPLOAD_TEMP_PREFIX.len() + TEMP_ID_LEN, "{temp}");

        let uploaded = finish(upload).await.unwrap();
        assert_eq!(
            uploaded,
            UploadedFile {
                name: "a.txt".into(),
                size: 5,
                renamed: false
            }
        );
        assert_eq!(fs::read(root.path().join("a.txt")).unwrap(), b"hello");
        assert_eq!(names(root.path()), ["a.txt"], "the temp is gone");
    }

    #[tokio::test]
    async fn empty_content_yields_an_empty_file() {
        let root = tempfile::tempdir().unwrap();
        let upload = begin(root.path(), root.path(), "empty.bin").await.unwrap();
        let uploaded = finish(upload).await.unwrap();

        assert_eq!(uploaded.size, 0);
        assert_eq!(fs::metadata(root.path().join("empty.bin")).unwrap().len(), 0);
        assert_eq!(names(root.path()), ["empty.bin"]);
    }

    #[tokio::test]
    async fn dropping_an_unfinished_upload_removes_the_temp() {
        let root = tempfile::tempdir().unwrap();
        let mut upload = begin(root.path(), root.path(), "partial.bin").await.unwrap();
        upload.write_chunk(&[7u8; 600]).await.unwrap();
        assert_eq!(temps(root.path()).len(), 1);

        drop(upload);
        assert!(names(root.path()).is_empty(), "{:?}", names(root.path()));
    }

    #[tokio::test]
    async fn content_past_the_cap_is_refused_and_the_temp_removed() {
        let root = tempfile::tempdir().unwrap();
        let mut upload = begin(root.path(), root.path(), "big.bin").await.unwrap();
        // Exactly at the cap is allowed.
        upload.write_chunk(&[1u8; 1000]).await.unwrap();
        upload.write_chunk(&[2u8; 24]).await.unwrap();

        let err = upload.write_chunk(&[3u8; 1]).await.unwrap_err();
        assert!(matches!(err, FileError::PayloadTooLarge(_)), "got {err:?}");

        drop(upload);
        assert!(names(root.path()).is_empty(), "{:?}", names(root.path()));
    }

    #[tokio::test]
    async fn content_at_the_cap_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let mut upload = begin(root.path(), root.path(), "full.bin").await.unwrap();
        upload.write_chunk(&[9u8; LIMIT as usize]).await.unwrap();
        let uploaded = finish(upload).await.unwrap();

        assert_eq!(uploaded.size, LIMIT);
        assert_eq!(fs::metadata(root.path().join("full.bin")).unwrap().len(), LIMIT);
    }

    #[tokio::test]
    async fn a_taken_name_moves_to_the_next_free_candidate() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a.txt"), "first").unwrap();
        fs::write(root.path().join("a copy.txt"), "second").unwrap();
        // A directory holds a name just as a file does.
        fs::create_dir(root.path().join("assets")).unwrap();

        let mut upload = begin(root.path(), root.path(), "a.txt").await.unwrap();
        upload.write_chunk(b"third").await.unwrap();
        let uploaded = finish(upload).await.unwrap();
        assert_eq!(uploaded.name, "a copy 2.txt");
        assert!(uploaded.renamed);

        let dir_clash = begin(root.path(), root.path(), "assets").await.unwrap();
        let uploaded = finish(dir_clash).await.unwrap();
        assert_eq!(uploaded.name, "assets copy");
        assert!(uploaded.renamed);

        // Nothing that existed was touched.
        assert_eq!(fs::read_to_string(root.path().join("a.txt")).unwrap(), "first");
        assert_eq!(fs::read_to_string(root.path().join("a copy.txt")).unwrap(), "second");
        assert_eq!(fs::read_to_string(root.path().join("a copy 2.txt")).unwrap(), "third");
        assert!(root.path().join("assets").is_dir());
        assert!(temps(root.path()).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_dangling_symlink_holds_its_name_and_is_not_followed() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("project");
        fs::create_dir(&root).unwrap();
        let target = base.path().join("missing-target.txt");
        std::os::unix::fs::symlink(&target, root.join("a.txt")).unwrap();

        let mut upload = begin(&root, &root, "a.txt").await.unwrap();
        upload.write_chunk(b"x").await.unwrap();
        let uploaded = finish(upload).await.unwrap();

        assert_eq!(uploaded.name, "a copy.txt");
        assert!(!target.exists(), "nothing may be created at the link target");
        assert!(
            fs::symlink_metadata(root.join("a.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_uploads_of_one_name_never_share_a_file() {
        let root = tempfile::tempdir().unwrap();
        let tasks: Vec<_> = (0..16u8)
            .map(|i| {
                let dir = root.path().to_path_buf();
                tokio::spawn(async move {
                    let mut upload = begin(&dir, &dir, "same.bin").await.unwrap();
                    upload.write_chunk(&[i; 64]).await.unwrap();
                    (i, finish(upload).await.unwrap().name)
                })
            })
            .collect();

        let mut seen = std::collections::HashSet::new();
        for task in tasks {
            let (i, name) = task.await.unwrap();
            assert!(seen.insert(name.clone()), "name {name} was handed out twice");
            assert_eq!(fs::read(root.path().join(&name)).unwrap(), vec![i; 64], "{name}");
        }
        assert_eq!(names(root.path()).len(), 16, "{:?}", names(root.path()));
    }

    #[tokio::test]
    async fn invalid_names_are_refused_before_touching_the_disk() {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "",
            " ",
            ".",
            "..",
            "a/b",
            "a\\b",
            "a\0b",
            "a\tb.txt",
            "line\nbreak",
            "bell\u{7}",
        ] {
            let message = bad_request_message(begin(root.path(), root.path(), name).await);
            assert_eq!(message, "invalid file name", "{name:?}");
        }
        let message = bad_request_message(begin(root.path(), root.path(), &"n".repeat(256)).await);
        assert_eq!(message, "file name is too long");
        // 255 bytes is still one valid component.
        let upload = begin(root.path(), root.path(), &"n".repeat(255)).await.unwrap();
        drop(upload);

        assert!(names(root.path()).is_empty(), "{:?}", names(root.path()));
    }

    #[test]
    fn windows_device_names_and_characters_are_recognized() {
        for name in [
            "a<b",
            "a>b",
            "a:b",
            "a\"b",
            "a|b",
            "a?b",
            "a*b",
            "trailing.",
            "trailing ",
            "CON",
            "con",
            "Con.txt",
            "nul.tar.gz",
            "AUX",
            "prn.md",
            "COM1",
            "com9.log",
            "LPT1",
            "lpt3.txt",
            "COM\u{b9}",
            "lpt\u{b2}.txt",
            "COM0",
        ] {
            assert!(!windows_can_store(name), "{name:?} must be refused on Windows");
        }
        for name in [
            "CONSOLE.txt",
            "com10",
            "LPT",
            "nul_notes.md",
            "report.txt",
            "a.b.c",
            ".env",
            "COMa",
        ] {
            assert!(windows_can_store(name), "{name:?} is a fine Windows name");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_refuses_what_it_cannot_store() {
        for name in ["CON.txt", "a:b.txt", "trailing.", "q?.md"] {
            assert!(validate_file_name(name).is_err(), "{name:?}");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn other_platforms_keep_names_windows_would_refuse() {
        for name in ["CON.txt", "a:b.txt", "q?.md"] {
            assert_eq!(validate_file_name(name).unwrap(), name);
        }
    }

    #[tokio::test]
    async fn the_target_must_be_an_existing_directory() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("file.txt"), "x").unwrap();

        let missing = bad_request_message(begin(root.path(), &root.path().join("nope"), "a.txt").await);
        assert_eq!(missing, "upload target directory does not exist");

        let file = bad_request_message(begin(root.path(), &root.path().join("file.txt"), "a.txt").await);
        assert_eq!(file, "upload target is not a directory");

        let under_file = bad_request_message(begin(root.path(), &root.path().join("file.txt/sub"), "a.txt").await);
        assert_eq!(under_file, "upload target is not a directory");

        assert_eq!(names(root.path()), ["file.txt"]);
    }

    #[tokio::test]
    async fn target_paths_no_filesystem_takes_are_client_errors() {
        let root = tempfile::tempdir().unwrap();

        let nul = bad_request_message(begin(root.path(), &root.path().join("a\0b"), "a.txt").await);
        assert_eq!(nul, "invalid upload target path");

        let long_segment = root.path().join("s".repeat(256));
        let message = bad_request_message(begin(root.path(), &long_segment, "a.txt").await);
        assert_eq!(message, "upload target path is too long");

        let long_path = root.path().join("d/".repeat(2100));
        let message = bad_request_message(begin(root.path(), &long_path, "a.txt").await);
        assert_eq!(message, "upload target path is too long");

        assert!(names(root.path()).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_loop_in_the_target_is_a_client_error() {
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(root.path().join("loop"), root.path().join("loop")).unwrap();

        let message = bad_request_message(begin(root.path(), &root.path().join("loop"), "a.txt").await);
        assert_eq!(message, "upload target path has a symlink loop");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_directory_must_stay_inside_the_root() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("project");
        let outside = base.path().join("outside");
        fs::create_dir_all(root.join("real")).unwrap();
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();

        let escaped = bad_request_message(begin(&root, &root.join("escape"), "a.txt").await);
        assert_eq!(escaped, "upload target is outside the project folder");
        assert!(names(&outside).is_empty(), "nothing may be written outside the root");

        // A link that stays inside the root is an ordinary directory.
        let upload = begin(&root, &root.join("alias"), "a.txt").await.unwrap();
        finish(upload).await.unwrap();
        assert!(root.join("real").join("a.txt").is_file());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_directory_without_write_permission_is_forbidden() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let locked = root.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        // Root (or CAP_DAC_OVERRIDE) writes anyway: then there is nothing to check.
        let probe = locked.join("probe");
        if fs::write(&probe, b"x").is_ok() {
            fs::remove_file(&probe).unwrap();
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipped: permissions are not enforced for this user");
            return;
        }

        let result = begin(root.path(), &locked, "a.txt").await;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        match result {
            Err(FileError::Forbidden(message)) => assert_eq!(message, "permission denied"),
            Err(other) => panic!("expected Forbidden, got {other:?}"),
            Ok(_) => panic!("expected Forbidden, got an upload"),
        }
        assert!(names(&locked).is_empty());
    }

    #[test]
    fn io_failures_map_permission_problems_to_forbidden() {
        for kind in [ErrorKind::PermissionDenied, ErrorKind::ReadOnlyFilesystem] {
            let err = io_failure(&io::Error::from(kind), "ctx");
            assert!(matches!(err, FileError::Forbidden(_)), "{kind:?} -> {err:?}");
        }
        let err = io_failure(&io::Error::from(ErrorKind::StorageFull), "ctx");
        assert!(matches!(err, FileError::Internal(_)), "{err:?}");
    }

    #[test]
    fn exhausted_candidates_are_a_conflict() {
        let root = tempfile::tempdir().unwrap();
        for name in ["a.txt", "a copy.txt", "a copy 2.txt"] {
            fs::write(root.path().join(name), "taken").unwrap();
        }
        let temp = root.path().join(".aionui-upload-test");
        fs::write(&temp, "new").unwrap();

        let err = claim_name_sync(root.path(), &temp, "a.txt", 3, true).unwrap_err();
        assert!(matches!(err, FileError::Conflict(_)), "{err:?}");
        assert_eq!(fs::read_to_string(root.path().join("a.txt")).unwrap(), "taken");
    }

    #[test]
    fn the_placeholder_claim_renames_the_temp_over_its_own_placeholder() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a.txt"), "taken").unwrap();
        let temp = root.path().join(".aionui-upload-test");
        fs::write(&temp, "new").unwrap();

        let name = claim_name_sync(root.path(), &temp, "a.txt", MAX_NAME_ATTEMPTS, false).unwrap();

        assert_eq!(name, "a copy.txt");
        assert_eq!(fs::read_to_string(root.path().join("a copy.txt")).unwrap(), "new");
        assert_eq!(fs::read_to_string(root.path().join("a.txt")).unwrap(), "taken");
        assert!(!temp.exists(), "the rename moved the temp");
    }

    #[test]
    fn fitted_candidates_match_the_copy_naming_and_stay_within_255_bytes() {
        // Ordinary names: exactly the drag-import naming.
        for attempt in 0..4 {
            assert_eq!(
                fitted_candidate("report.txt", attempt),
                candidate_name("report.txt", attempt, false)
            );
        }
        // A 255-byte name keeps its extension and suffix; the stem gives way.
        let long = format!("{}.txt", "n".repeat(251));
        assert_eq!(fitted_candidate(&long, 0), long);
        let first = fitted_candidate(&long, 1);
        assert_eq!(first, format!("{} copy.txt", "n".repeat(246)));
        let later = fitted_candidate(&long, 9_999);
        assert!(later.len() <= 255 && later.ends_with(" copy 9999.txt"), "{later}");
        // Multi-byte stems are cut at a character boundary.
        let wide = format!("{}.md", "\u{6587}".repeat(84));
        for attempt in [1, 2, 9_999] {
            let fitted = fitted_candidate(&wide, attempt);
            assert!(fitted.len() <= 255, "{attempt}: {} bytes", fitted.len());
            assert!(fitted.ends_with(".md"), "{fitted}");
        }
        // An extension too long to keep beside the suffix is shortened with the stem.
        let long_ext = format!("a.{}", "e".repeat(250));
        let fitted = fitted_candidate(&long_ext, 3);
        assert!(fitted.len() <= 255 && fitted.ends_with(" copy 3"), "{fitted}");
    }

    #[tokio::test]
    async fn a_long_name_that_collides_is_shortened_instead_of_failing() {
        let root = tempfile::tempdir().unwrap();
        let long = format!("{}.txt", "n".repeat(251));
        fs::write(root.path().join(&long), "first").unwrap();

        let mut upload = begin(root.path(), root.path(), &long).await.unwrap();
        upload.write_chunk(b"second").await.unwrap();
        let uploaded = finish(upload).await.unwrap();

        assert!(uploaded.renamed);
        assert_eq!(uploaded.name, format!("{} copy.txt", "n".repeat(246)));
        assert_eq!(fs::read_to_string(root.path().join(&uploaded.name)).unwrap(), "second");
    }

    #[tokio::test]
    async fn temps_a_crash_left_behind_are_swept_after_a_day() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join(format!("{PROJECT_UPLOAD_TEMP_PREFIX}old"));
        let fresh = root.path().join(format!("{PROJECT_UPLOAD_TEMP_PREFIX}fresh"));
        fs::write(&old, "left by a crash").unwrap();
        fs::write(&fresh, "another upload in progress").unwrap();
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(25 * 60 * 60))
            .unwrap();

        let upload = begin(root.path(), root.path(), "a.txt").await.unwrap();
        assert!(!old.exists(), "a temp older than a day is removed");
        assert!(fresh.exists(), "a recent temp may belong to a running upload");
        drop(upload);

        assert_eq!(names(root.path()), [format!("{PROJECT_UPLOAD_TEMP_PREFIX}fresh")]);
    }

    #[tokio::test]
    async fn the_listing_cache_is_cleared_when_the_upload_lands_or_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let key = fs::canonicalize(root.path()).unwrap().to_string_lossy().into_owned();
        let cache: ListingCache = Arc::new(DashMap::new());

        cache.insert(key.clone(), Vec::new());
        let mut upload = LocalProjectUpload::begin(root.path(), root.path(), "a.txt", LIMIT, cache.clone())
            .await
            .unwrap();
        upload.write_chunk(b"x").await.unwrap();
        assert!(cache.contains_key(&key), "starting an upload leaves the listing alone");
        finish(upload).await.unwrap();
        assert!(!cache.contains_key(&key), "the landed file clears the listing");

        cache.insert(key.clone(), Vec::new());
        let upload = LocalProjectUpload::begin(root.path(), root.path(), "b.txt", LIMIT, cache.clone())
            .await
            .unwrap();
        assert!(cache.contains_key(&key));
        drop(upload);
        assert!(!cache.contains_key(&key), "removing the temp clears the listing");
    }
}
