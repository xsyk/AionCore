//! Streaming one upload into a project directory (`POST /api/fs/upload-to-project`).
//!
//! The route resolves the pe-addressed target (ownership + lexical containment via
//! `resolve_reference`) and feeds the multipart body through
//! [`ProjectUpload::write_chunk`]. Everything that touches the disk lives here: the
//! realpath guard on the target directory, the never-overwrite name reservation,
//! the size cap, and removing the partial file on every path that does not reach
//! [`ProjectUpload::finish`] — an error, a rejected request, or a dropped request
//! future alike.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncWriteExt, BufWriter};

use super::candidate_name;
use crate::error::FileError;
use crate::types::UploadedFile;

/// Longest accepted file name, in UTF-8 bytes — the usual filesystem limit for one
/// path component (the same bound folder creation uses).
const MAX_FILE_NAME_BYTES: usize = 255;

/// Candidate names tried before giving up (the drag-import copy uses the same cap).
const MAX_NAME_ATTEMPTS: usize = 10_000;

/// Buffer between the incoming body chunks and the file, so small chunks do not
/// each cost a blocking-pool round trip.
const WRITE_BUFFER_BYTES: usize = 256 * 1024;

/// An upload in progress: a freshly created file that is removed again unless
/// [`finish`](Self::finish) completes.
pub struct ProjectUpload {
    // Declared before `guard` so the file handle is released before the guard
    // removes the path.
    writer: BufWriter<tokio::fs::File>,
    guard: PartialFile,
    name: String,
    renamed: bool,
    written: u64,
    limit: u64,
}

impl ProjectUpload {
    /// Check `dir`, reserve a free name for `file_name` in it and open the file.
    ///
    /// `root` is the folder root of the entry `dir` was resolved from; `limit` caps
    /// the content in bytes. Also returns the root's real path (the key of the
    /// workspace listing cache).
    pub(super) async fn begin(
        root: &Path,
        dir: &Path,
        file_name: &str,
        limit: u64,
    ) -> Result<(Self, PathBuf), FileError> {
        validate_file_name(file_name)?;
        let (root, dir, requested) = (root.to_path_buf(), dir.to_path_buf(), file_name.to_owned());
        // The guard travels inside the task's output, so even if this future is
        // dropped while the task runs, the file it creates is removed with it.
        let (file, guard, name, root_real) = tokio::task::spawn_blocking(move || {
            let (root_real, dir_real) = upload_dir_sync(&root, &dir)?;
            let (file, guard, name) = reserve_file_sync(&dir_real, &requested)?;
            Ok::<_, FileError>((file, guard, name, root_real))
        })
        .await
        .map_err(|e| FileError::Internal(format!("upload task failed: {e}")))??;

        let upload = Self {
            writer: BufWriter::with_capacity(WRITE_BUFFER_BYTES, tokio::fs::File::from_std(file)),
            guard,
            renamed: name != file_name,
            name,
            written: 0,
            limit,
        };
        Ok((upload, root_real))
    }

    /// Append `chunk`. Fails with `PayloadTooLarge`, writing nothing, once the
    /// content would pass the size cap.
    pub async fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), FileError> {
        let len = chunk.len() as u64;
        if self.written.saturating_add(len) > self.limit {
            return Err(FileError::PayloadTooLarge(format!(
                "file exceeds the upload limit of {} bytes",
                self.limit
            )));
        }
        self.writer.write_all(chunk).await.map_err(write_error)?;
        self.written += len;
        Ok(())
    }

    /// Flush the content and keep the file.
    pub async fn finish(mut self) -> Result<UploadedFile, FileError> {
        // `BufWriter::flush` also waits for the file's in-flight write, so write
        // errors surface here rather than being lost when the handle drops.
        self.writer.flush().await.map_err(write_error)?;
        let Self {
            writer,
            guard,
            name,
            renamed,
            written,
            ..
        } = self;
        drop(writer);
        guard.keep();
        Ok(UploadedFile {
            name,
            size: written,
            renamed,
        })
    }
}

/// Removes a file that was created for an upload when dropped, unless the upload
/// was kept.
struct PartialFile {
    path: Option<PathBuf>,
}

impl PartialFile {
    fn keep(mut self) {
        self.path = None;
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != ErrorKind::NotFound
        {
            tracing::warn!(path = %path.display(), error = %e, "fs: could not remove a partially uploaded file");
        }
    }
}

/// An upload name must be exactly one plain path component.
fn validate_file_name(name: &str) -> Result<(), FileError> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return Err(FileError::BadRequest("invalid file name".to_owned()));
    }
    if name.len() > MAX_FILE_NAME_BYTES {
        return Err(FileError::BadRequest("file name is too long".to_owned()));
    }
    Ok(())
}

/// Resolve the upload directory to its real path: it must be an existing
/// directory that stays inside the folder root's real path. `resolve_reference`
/// checks containment lexically only, so without this a symlink inside the project
/// could aim the write anywhere on the host — the same access-time guard the
/// Explorer's own create/copy commands apply. Returns `(root_real, dir_real)`.
///
/// Messages are path-free: the client addressed the directory by identity.
fn upload_dir_sync(root: &Path, dir: &Path) -> Result<(PathBuf, PathBuf), FileError> {
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

fn upload_dir_error(e: &std::io::Error) -> FileError {
    match e.kind() {
        ErrorKind::NotFound => FileError::BadRequest("upload target directory does not exist".to_owned()),
        // A path component is a file (`a.txt/sub`).
        ErrorKind::NotADirectory => FileError::BadRequest("upload target is not a directory".to_owned()),
        _ => FileError::Internal(format!("cannot resolve upload target: {e}")),
    }
}

/// Create the first free candidate for `requested` in `dir` (`name`, `name copy`,
/// `name copy 2`, … — the drag-import naming). `create_new` makes "is it free?"
/// and "take it" one atomic step, so concurrent uploads of the same name never
/// claim the same file and an existing entry is never truncated.
fn reserve_file_sync(dir: &Path, requested: &str) -> Result<(std::fs::File, PartialFile, String), FileError> {
    for attempt in 0..MAX_NAME_ATTEMPTS {
        let name = candidate_name(requested, attempt, false);
        let path = dir.join(&name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((file, PartialFile { path: Some(path) }, name)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            // A directory holding the name may surface as access denied rather
            // than `AlreadyExists` on some platforms; it is taken either way.
            Err(e) if e.kind() == ErrorKind::PermissionDenied && path.is_dir() => {}
            Err(e) => return Err(FileError::Internal(format!("cannot create uploaded file: {e}"))),
        }
    }
    Err(FileError::Internal(
        "too many name collisions in the upload target".to_owned(),
    ))
}

fn write_error(e: std::io::Error) -> FileError {
    FileError::Internal(format!("cannot write uploaded file: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const LIMIT: u64 = 1024;

    async fn begin(root: &Path, dir: &Path, name: &str) -> Result<ProjectUpload, FileError> {
        ProjectUpload::begin(root, dir, name, LIMIT)
            .await
            .map(|(upload, _)| upload)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn bad_request_message(result: Result<ProjectUpload, FileError>) -> String {
        match result {
            Err(FileError::BadRequest(message)) => message,
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got an upload"),
        }
    }

    #[tokio::test]
    async fn streams_chunks_into_a_new_file_and_keeps_it_on_finish() {
        let root = tempfile::tempdir().unwrap();
        let mut upload = begin(root.path(), root.path(), "a.txt").await.unwrap();
        upload.write_chunk(b"hel").await.unwrap();
        upload.write_chunk(b"lo").await.unwrap();
        let uploaded = upload.finish().await.unwrap();

        assert_eq!(
            uploaded,
            UploadedFile {
                name: "a.txt".into(),
                size: 5,
                renamed: false
            }
        );
        assert_eq!(fs::read(root.path().join("a.txt")).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn empty_content_yields_an_empty_file() {
        let root = tempfile::tempdir().unwrap();
        let upload = begin(root.path(), root.path(), "empty.bin").await.unwrap();
        let uploaded = upload.finish().await.unwrap();

        assert_eq!(uploaded.size, 0);
        assert_eq!(fs::metadata(root.path().join("empty.bin")).unwrap().len(), 0);
    }

    #[tokio::test]
    async fn dropping_an_unfinished_upload_removes_the_file() {
        let root = tempfile::tempdir().unwrap();
        let mut upload = begin(root.path(), root.path(), "partial.bin").await.unwrap();
        upload.write_chunk(&[7u8; 600]).await.unwrap();
        assert!(root.path().join("partial.bin").exists(), "reserved while in progress");

        drop(upload);
        assert!(names(root.path()).is_empty(), "partial file must be removed");
    }

    #[tokio::test]
    async fn content_past_the_cap_is_refused_and_the_file_removed() {
        let root = tempfile::tempdir().unwrap();
        let mut upload = begin(root.path(), root.path(), "big.bin").await.unwrap();
        // Exactly at the cap is allowed.
        upload.write_chunk(&[1u8; 1000]).await.unwrap();
        upload.write_chunk(&[2u8; 24]).await.unwrap();

        let err = upload.write_chunk(&[3u8; 1]).await.unwrap_err();
        assert!(matches!(err, FileError::PayloadTooLarge(_)), "got {err:?}");

        drop(upload);
        assert!(
            names(root.path()).is_empty(),
            "over-cap upload must leave nothing behind"
        );
    }

    #[tokio::test]
    async fn content_at_the_cap_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let mut upload = begin(root.path(), root.path(), "full.bin").await.unwrap();
        upload.write_chunk(&[9u8; LIMIT as usize]).await.unwrap();
        let uploaded = upload.finish().await.unwrap();

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
        let uploaded = upload.finish().await.unwrap();
        assert_eq!(uploaded.name, "a copy 2.txt");
        assert!(uploaded.renamed);

        let dir_clash = begin(root.path(), root.path(), "assets").await.unwrap();
        let uploaded = dir_clash.finish().await.unwrap();
        assert_eq!(uploaded.name, "assets copy");
        assert!(uploaded.renamed);

        // Nothing that existed was touched.
        assert_eq!(fs::read_to_string(root.path().join("a.txt")).unwrap(), "first");
        assert_eq!(fs::read_to_string(root.path().join("a copy.txt")).unwrap(), "second");
        assert_eq!(fs::read_to_string(root.path().join("a copy 2.txt")).unwrap(), "third");
        assert!(root.path().join("assets").is_dir());
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
                    (i, upload.finish().await.unwrap().name)
                })
            })
            .collect();

        let mut seen = std::collections::HashSet::new();
        for task in tasks {
            let (i, name) = task.await.unwrap();
            assert!(seen.insert(name.clone()), "name {name} was handed out twice");
            assert_eq!(fs::read(root.path().join(&name)).unwrap(), vec![i; 64], "{name}");
        }
        assert_eq!(names(root.path()).len(), 16);
    }

    #[tokio::test]
    async fn invalid_names_are_refused_before_touching_the_disk() {
        let root = tempfile::tempdir().unwrap();
        for name in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
            let message = bad_request_message(begin(root.path(), root.path(), name).await);
            assert_eq!(message, "invalid file name", "{name:?}");
        }
        let message = bad_request_message(begin(root.path(), root.path(), &"n".repeat(256)).await);
        assert_eq!(message, "file name is too long");
        // 255 bytes is still one valid component.
        let upload = begin(root.path(), root.path(), &"n".repeat(255)).await.unwrap();
        drop(upload);

        assert!(names(root.path()).is_empty());
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
        upload.finish().await.unwrap();
        assert!(root.join("real").join("a.txt").is_file());
    }
}
