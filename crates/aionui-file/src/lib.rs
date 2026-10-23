#![warn(clippy::disallowed_types)]

//! File system operations: read/write, path safety, and snapshots.
pub mod error;
pub mod path_safety;
pub mod routes;
pub mod service;
pub mod snapshot_service;
pub mod traits;
pub mod types;

pub use error::FileError;
pub use path_safety::{has_traversal, validate_path, validate_path_for_write};
pub use routes::{
    FileRouterState, PROJECT_UPLOAD_IDLE_TIMEOUT, PROJECT_UPLOAD_QUEUE_WAIT, PROJECT_UPLOAD_SLOTS, ProjectUploadLimits,
    file_routes,
};
pub use service::FileService;
pub use snapshot_service::SnapshotService;
pub use traits::{
    ClipboardWriterRef, FileServiceRef, IClipboardWriter, IFileService, IItemRevealer, IProjectUpload,
    ISnapshotService, ISystemFileOpener, ItemRevealerRef, ProjectUploadRef, SnapshotServiceRef, SystemFileOpenerRef,
};
pub use types::{
    CompareResult, ContentUpdateEvent, ContentUpdateOperation, CopyResult, DirOrFile, FileChangeInfo, FileMetadata,
    SnapshotInfo, SnapshotMode, UploadedFile, WorkspaceFlatFile,
};
