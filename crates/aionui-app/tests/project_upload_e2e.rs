//! E2E tests for `POST /api/fs/upload-to-project` — uploading one file into a
//! directory of a project entry (the Explorer's "upload file").

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use tower::ServiceExt;

use aionui_app::{AppConfig, AppServices, build_module_states, create_router_with_states};
use aionui_common::constants::{PROJECT_UPLOAD_MAX_SIZE, PROJECT_UPLOAD_TEMP_PREFIX};
use aionui_file::ProjectUploadLimits;
use common::{body_json, build_app, setup_and_login};

const URI: &str = "/api/fs/upload-to-project";
const BOUNDARY: &str = "----AionProjectUploadBoundary7QpL";
const MIB: usize = 1024 * 1024;
/// More than the upload's 256 KiB write buffer, so the bytes reach the disk.
const PAST_THE_WRITE_BUFFER: usize = 300 * 1024;

// ===========================================================================
// Helpers
// ===========================================================================

/// A `multipart/form-data` body whose parts keep the order they are added in —
/// the endpoint's contract is order-sensitive.
#[derive(Default)]
struct Form {
    body: Vec<u8>,
}

impl Form {
    fn text(mut self, name: &str, value: &str) -> Self {
        self.body.extend_from_slice(text_part_head(name).as_bytes());
        self.body.extend_from_slice(value.as_bytes());
        self.body.extend_from_slice(b"\r\n");
        self
    }

    /// `pe_id` then `relative_path`, as the contract orders them.
    fn target(self, pe_id: &str, relative_path: &str) -> Self {
        self.text("pe_id", pe_id).text("relative_path", relative_path)
    }

    /// A `file` part named by its Content-Disposition `filename`.
    fn file(self, filename: &str, data: &[u8]) -> Self {
        self.file_part(&format!("; filename=\"{filename}\""), data)
    }

    /// A `file` part without any `filename` attribute.
    fn unnamed_file(self, data: &[u8]) -> Self {
        self.file_part("", data)
    }

    fn file_part(mut self, disposition_suffix: &str, data: &[u8]) -> Self {
        self.body
            .extend_from_slice(file_part_head(disposition_suffix).as_bytes());
        self.body.extend_from_slice(data);
        self.body.extend_from_slice(b"\r\n");
        self
    }

    fn build(mut self) -> Vec<u8> {
        self.body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        self.body
    }
}

fn text_part_head(name: &str) -> String {
    format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n")
}

fn file_part_head(disposition_suffix: &str) -> String {
    format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"{disposition_suffix}\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    )
}

/// `head`, then `filler_len` bytes streamed in 1 MiB chunks, then `tail` — so the
/// size-limit tests never hold their payload in memory. Returns the body and its
/// exact length.
fn streamed_body(head: Vec<u8>, filler_len: usize, tail: Vec<u8>) -> (Body, usize) {
    let total = head.len() + filler_len + tail.len();
    let chunk = Bytes::from(vec![0x5A_u8; MIB]);
    let mut frames: Vec<Result<Bytes, std::io::Error>> = vec![Ok(Bytes::from(head))];
    let mut remaining = filler_len;
    while remaining > 0 {
        let n = remaining.min(MIB);
        frames.push(Ok(chunk.slice(..n)));
        remaining -= n;
    }
    frames.push(Ok(Bytes::from(tail)));
    (Body::from_stream(futures_util::stream::iter(frames)), total)
}

/// A complete upload of a `file_len`-byte `big.bin` into the entry root.
fn streamed_upload(pe_id: &str, file_len: usize) -> (Body, usize) {
    let tail = format!("\r\n--{BOUNDARY}--\r\n").into_bytes();
    streamed_body(upload_head(pe_id, "", "big.bin").to_vec(), file_len, tail)
}

/// Everything of an upload before the file's content: the target parts and the
/// `file` part's headers.
fn upload_head(pe_id: &str, relative_path: &str, filename: &str) -> Bytes {
    let mut head = Form::default().target(pe_id, relative_path).body;
    head.extend_from_slice(file_part_head(&format!("; filename=\"{filename}\"")).as_bytes());
    Bytes::from(head)
}

/// Everything of an upload after the file's content.
fn upload_tail() -> Bytes {
    Bytes::from(format!("\r\n--{BOUNDARY}--\r\n"))
}

/// A request body the test feeds frame by frame. Dropping the sender ends the
/// body; sending an `Err` breaks it the way a dropped connection does.
type BodyFeed = mpsc::UnboundedSender<Result<Bytes, std::io::Error>>;

fn fed_body() -> (BodyFeed, Body) {
    let (feed, frames) = mpsc::unbounded_channel();
    let stream = futures_util::stream::unfold(frames, |mut frames| async move {
        frames.recv().await.map(|frame| (frame, frames))
    });
    (feed, Body::from_stream(stream))
}

/// Credentials attached to a request; `None` leaves that header off.
#[derive(Clone, Copy)]
struct Auth<'a> {
    token: Option<&'a str>,
    csrf: Option<&'a str>,
    /// The `x-aionui-act-as` target, for the super admin.
    act_as: Option<&'a str>,
}

fn upload_request(body: Body, content_length: Option<usize>, auth: Auth<'_>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(URI)
        .header("content-type", format!("multipart/form-data; boundary={BOUNDARY}"));
    if let Some(len) = content_length {
        builder = builder.header("content-length", len);
    }
    if let Some(token) = auth.token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(csrf) = auth.csrf {
        builder = builder
            .header("x-csrf-token", csrf)
            .header("cookie", format!("aionui-csrf-token={csrf}"));
    }
    if let Some(target) = auth.act_as {
        builder = builder.header("x-aionui-act-as", target);
    }
    builder.body(body).unwrap()
}

async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    (status, body_json(response).await)
}

/// A logged-in admin owning one project entry whose folder is `workspace`. The
/// folder is nested in a scratch directory so "nothing escaped" checks stay
/// inside the test.
struct Fixture {
    app: axum::Router,
    services: AppServices,
    token: String,
    csrf: String,
    scratch: tempfile::TempDir,
    workspace: PathBuf,
    pe_id: String,
}

impl Fixture {
    async fn new() -> Self {
        let (app, services) = build_app().await;
        Self::log_in(app, services).await
    }

    /// A fixture whose upload route runs with `limits`.
    async fn with_limits(limits: ProjectUploadLimits) -> Self {
        let db = aionui_db::init_database_memory().await.unwrap();
        let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
        let (mut states, _) = build_module_states(&services).await.expect("build module states");
        states.file.project_upload = limits;
        let app = create_router_with_states(&services, states);
        Self::log_in(app, services).await
    }

    async fn log_in(mut app: axum::Router, services: AppServices) -> Self {
        let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
        let scratch = tempfile::tempdir().unwrap();
        let workspace = scratch.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let pe_id = register_folder(&services, "system_default_user", &workspace).await;
        Self {
            app,
            services,
            token,
            csrf,
            scratch,
            workspace,
            pe_id,
        }
    }

    fn auth(&self) -> Auth<'_> {
        Auth {
            token: Some(&self.token),
            csrf: Some(&self.csrf),
            act_as: None,
        }
    }

    /// Send `form` as the logged-in admin.
    async fn upload(&self, form: Form) -> (StatusCode, Value) {
        self.upload_as(form, self.auth()).await
    }

    async fn upload_as(&self, form: Form, auth: Auth<'_>) -> (StatusCode, Value) {
        let body = form.build();
        let len = body.len();
        send(&self.app, upload_request(Body::from(body), Some(len), auth)).await
    }

    /// Start an upload whose body the test feeds (see [`fed_body`]), as the
    /// logged-in admin. The request runs in the background.
    fn start_fed_upload(&self) -> (BodyFeed, JoinHandle<(StatusCode, Value)>) {
        let (feed, body) = fed_body();
        let request = upload_request(body, None, self.auth());
        let app = self.app.clone();
        (feed, tokio::spawn(async move { send(&app, request).await }))
    }

    /// `POST /api/fs/list` of the workspace: the relative paths, sorted.
    async fn list(&self) -> Vec<String> {
        let request = Request::builder()
            .method("POST")
            .uri("/api/fs/list")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.token))
            .header("x-csrf-token", &self.csrf)
            .header("cookie", format!("aionui-csrf-token={}", self.csrf))
            .body(Body::from(
                json!({ "root": self.workspace.to_str().unwrap() }).to_string(),
            ))
            .unwrap();
        let (status, json) = send(&self.app, request).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let mut paths: Vec<String> = json["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["relative_path"].as_str().unwrap().to_owned())
            .collect();
        paths.sort();
        paths
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.workspace.join(relative)
    }
}

/// Upload temps directly inside `dir`, with their sizes.
fn temps(dir: &Path) -> Vec<(String, u64)> {
    entries(dir)
        .into_iter()
        .filter(|name| name.starts_with(PROJECT_UPLOAD_TEMP_PREFIX))
        .map(|name| {
            let size = std::fs::metadata(dir.join(&name)).map(|m| m.len()).unwrap_or(0);
            (name, size)
        })
        .collect()
}

/// Poll until `check` holds; fails the test after five seconds.
async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Wait until an upload streaming into `dir` has put bytes on disk.
async fn wait_for_streaming(dir: &Path) {
    eventually("upload bytes in a temp file", || {
        temps(dir).iter().any(|(_, size)| *size > 0)
    })
    .await;
}

/// Register `dir` as a standard project of `user_id`; returns its entry's pe_id.
async fn register_folder(services: &AppServices, user_id: &str, dir: &Path) -> String {
    services
        .project_service
        .create_standard(user_id, aionui_project::canonical::to_file_uri(dir).unwrap())
        .await
        .unwrap()
        .project_explorer
        .pe_id
}

/// Names directly inside `dir`, sorted.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn assert_bad_request(status: StatusCode, json: &Value, message: &str) {
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["success"], false, "{json}");
    assert_eq!(json["code"], "BAD_REQUEST", "{json}");
    assert_eq!(json["error"], message, "{json}");
}

// ===========================================================================
// Happy paths
// ===========================================================================

#[tokio::test]
async fn uploads_a_file_into_the_entry_root() {
    let fx = Fixture::new().await;

    let (status, json) = fx
        .upload(Form::default().target(&fx.pe_id, "").file("hello.txt", b"hello world"))
        .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["success"], true);
    assert_eq!(
        json["data"],
        json!({ "name": "hello.txt", "relative_path": "hello.txt", "size": 11, "renamed": false })
    );
    assert_eq!(std::fs::read(fx.path("hello.txt")).unwrap(), b"hello world");
}

#[tokio::test]
async fn uploads_a_file_into_a_subdirectory() {
    let fx = Fixture::new().await;
    std::fs::create_dir_all(fx.path("docs/2026")).unwrap();

    let (status, json) = fx
        .upload(
            Form::default()
                .target(&fx.pe_id, "docs/2026")
                .file("report.pdf", b"%PDF-1.7"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json["data"],
        json!({ "name": "report.pdf", "relative_path": "docs/2026/report.pdf", "size": 8, "renamed": false })
    );
    assert_eq!(std::fs::read(fx.path("docs/2026/report.pdf")).unwrap(), b"%PDF-1.7");

    // The response path is normalized whatever form the target was sent in.
    let (status, json) = fx
        .upload(
            Form::default()
                .target(&fx.pe_id, "./docs/2026/")
                .file("notes.md", b"# notes"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["relative_path"], "docs/2026/notes.md");

    assert_eq!(entries(&fx.path("docs/2026")), ["notes.md", "report.pdf"]);
    assert_eq!(entries(&fx.workspace), ["docs"], "nothing lands in the root");
}

#[tokio::test]
async fn a_taken_name_is_renamed_and_the_existing_file_is_kept() {
    let fx = Fixture::new().await;
    std::fs::create_dir(fx.path("sub")).unwrap();
    let form = |data: &[u8]| Form::default().target(&fx.pe_id, "sub").file("a.png", data);

    let (status, first) = fx.upload(form(b"first")).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(
        first["data"],
        json!({ "name": "a.png", "relative_path": "sub/a.png", "size": 5, "renamed": false })
    );

    let (status, second) = fx.upload(form(b"second")).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(
        second["data"],
        json!({ "name": "a copy.png", "relative_path": "sub/a copy.png", "size": 6, "renamed": true })
    );

    let (status, third) = fx.upload(form(b"third")).await;
    assert_eq!(status, StatusCode::OK, "{third}");
    assert_eq!(third["data"]["name"], "a copy 2.png");
    assert_eq!(third["data"]["relative_path"], "sub/a copy 2.png");
    assert_eq!(third["data"]["renamed"], true);

    assert_eq!(
        std::fs::read(fx.path("sub/a.png")).unwrap(),
        b"first",
        "never overwritten"
    );
    assert_eq!(std::fs::read(fx.path("sub/a copy.png")).unwrap(), b"second");
    assert_eq!(std::fs::read(fx.path("sub/a copy 2.png")).unwrap(), b"third");
}

#[tokio::test]
async fn only_the_last_segment_of_the_file_name_is_used() {
    let fx = Fixture::new().await;
    std::fs::create_dir(fx.path("sub")).unwrap();

    let (status, json) = fx
        .upload(
            Form::default()
                .target(&fx.pe_id, "sub")
                .file("../../escape/x.txt", b"x"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json["data"],
        json!({ "name": "x.txt", "relative_path": "sub/x.txt", "size": 1, "renamed": false })
    );

    let (status, json) = fx
        .upload(
            Form::default()
                .target(&fx.pe_id, "sub")
                .file(r"C:\Users\me\y.txt", b"y"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["relative_path"], "sub/y.txt");

    // The `file_name` override is cut the same way, and wins over the filename.
    let (status, json) = fx
        .upload(
            Form::default()
                .target(&fx.pe_id, "sub")
                .text("file_name", "nested/dir/z.txt")
                .file("ignored.bin", b"z"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["relative_path"], "sub/z.txt");

    assert_eq!(entries(&fx.path("sub")), ["x.txt", "y.txt", "z.txt"]);
    assert_eq!(std::fs::read(fx.path("sub/x.txt")).unwrap(), b"x");
    assert_eq!(entries(fx.scratch.path()), ["workspace"], "nothing escaped the entry");
}

#[tokio::test]
async fn the_file_name_part_overrides_the_filename_and_blank_falls_back() {
    let fx = Fixture::new().await;

    let (status, json) = fx
        .upload(
            Form::default()
                .target(&fx.pe_id, "")
                .text("file_name", "renamed.txt")
                .file("original.txt", b"x"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["name"], "renamed.txt");
    assert_eq!(json["data"]["renamed"], false, "the requested name was free");

    let (status, json) = fx
        .upload(
            Form::default()
                .target(&fx.pe_id, "")
                .text("file_name", "  ")
                .file("fallback.txt", b"y"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["name"], "fallback.txt");

    assert_eq!(entries(&fx.workspace), ["fallback.txt", "renamed.txt"]);
}

#[tokio::test]
async fn an_empty_file_is_uploaded() {
    let fx = Fixture::new().await;

    let (status, json) = fx
        .upload(Form::default().target(&fx.pe_id, "").file("empty.txt", b""))
        .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json["data"],
        json!({ "name": "empty.txt", "relative_path": "empty.txt", "size": 0, "renamed": false })
    );
    assert_eq!(std::fs::metadata(fx.path("empty.txt")).unwrap().len(), 0);
}

#[tokio::test]
async fn binary_content_round_trips_byte_exact() {
    let fx = Fixture::new().await;
    // Every byte value, plus runs that look like multipart framing.
    let mut data: Vec<u8> = (0..=255u8).cycle().take(256 * 1024).collect();
    data.extend_from_slice(b"\r\n--\r\n\r\n--");
    data.extend_from_slice(&BOUNDARY.as_bytes()[..12]);
    data.extend_from_slice(b"\0\0\r\n");

    let (status, json) = fx
        .upload(Form::default().target(&fx.pe_id, "").file("blob.bin", &data))
        .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["size"], data.len());
    assert_eq!(std::fs::read(fx.path("blob.bin")).unwrap(), data);
}

#[tokio::test]
async fn content_exactly_at_the_cap_is_accepted() {
    let fx = Fixture::new().await;
    let (body, len) = streamed_upload(&fx.pe_id, PROJECT_UPLOAD_MAX_SIZE);

    let (status, json) = send(&fx.app, upload_request(body, Some(len), fx.auth())).await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"]["size"], PROJECT_UPLOAD_MAX_SIZE);
    assert_eq!(
        std::fs::metadata(fx.path("big.bin")).unwrap().len(),
        PROJECT_UPLOAD_MAX_SIZE as u64
    );
}

// ===========================================================================
// Bad input
// ===========================================================================

#[tokio::test]
async fn dot_nul_and_empty_names_are_rejected_without_creating_anything() {
    let fx = Fixture::new().await;
    let named = |name: &str| Form::default().target(&fx.pe_id, "").file(name, b"x");
    let overridden = |name: &str| {
        Form::default()
            .target(&fx.pe_id, "")
            .text("file_name", name)
            .file("ok.txt", b"x")
    };
    let cases = [
        ("filename '..'", named("..")),
        ("filename '.'", named(".")),
        ("filename ending in '..'", named("a/..")),
        ("filename ending in a separator", named("dir/")),
        ("empty filename", named("")),
        ("file_name '..'", overridden("..")),
        ("file_name with NUL", overridden("a\0b.txt")),
        // The folder-name rule: no control characters.
        ("filename with a tab", named("a\tb.txt")),
        ("file_name with a newline", overridden("line\nbreak.txt")),
        ("file_name with DEL", overridden("del\u{7f}.txt")),
    ];
    for (case, form) in cases {
        let (status, json) = fx.upload(form).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{case}: {json}");
        assert_eq!(json["code"], "BAD_REQUEST", "{case}");
        assert_eq!(json["error"], "invalid file name", "{case}");
    }

    let (status, json) = fx.upload(overridden(&"n".repeat(256))).await;
    assert_bad_request(status, &json, "file name is too long");

    assert!(entries(&fx.workspace).is_empty(), "{:?}", entries(&fx.workspace));
}

#[tokio::test]
async fn missing_or_repeated_parts_are_rejected() {
    let fx = Fixture::new().await;

    let (status, json) = fx.upload(Form::default().target(&fx.pe_id, "")).await;
    assert_bad_request(status, &json, "missing 'file' part");

    let (status, json) = fx
        .upload(Form::default().text("pe_id", &fx.pe_id).file("a.txt", b"a"))
        .await;
    assert_bad_request(status, &json, "'pe_id' and 'relative_path' must precede 'file'");

    let (status, json) = fx.upload(Form::default().target(" ", "").file("a.txt", b"a")).await;
    assert_bad_request(status, &json, "'pe_id' must not be empty");

    let (status, json) = fx
        .upload(Form::default().target(&fx.pe_id, "").unnamed_file(b"a"))
        .await;
    assert_bad_request(
        status,
        &json,
        "missing file name: send a filename with the 'file' part or a 'file_name' part",
    );

    let (status, json) = fx
        .upload(
            Form::default()
                .target(&fx.pe_id, "")
                .text("pe_id", &fx.pe_id)
                .file("a.txt", b"a"),
        )
        .await;
    assert_bad_request(status, &json, "duplicate 'pe_id' part");

    let (status, json) = fx
        .upload(
            Form::default()
                .target(&fx.pe_id, "")
                .text("file_name", &"n".repeat(8 * 1024 + 1))
                .file("a.txt", b"a"),
        )
        .await;
    assert_bad_request(status, &json, "'file_name' is too long");

    let not_multipart = Request::builder()
        .method("POST")
        .uri(URI)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", fx.token))
        .header("x-csrf-token", &fx.csrf)
        .header("cookie", format!("aionui-csrf-token={}", fx.csrf))
        .body(Body::from("{}"))
        .unwrap();
    let (status, json) = send(&fx.app, not_multipart).await;
    assert_bad_request(status, &json, "expected a multipart/form-data body");

    assert!(entries(&fx.workspace).is_empty(), "{:?}", entries(&fx.workspace));
}

#[tokio::test]
async fn a_file_before_the_target_parts_is_rejected_and_creates_nothing() {
    let fx = Fixture::new().await;

    let (status, json) = fx
        .upload(Form::default().file("early.txt", b"too soon").target(&fx.pe_id, ""))
        .await;

    assert_bad_request(status, &json, "'pe_id' and 'relative_path' must precede 'file'");
    assert!(entries(&fx.workspace).is_empty(), "{:?}", entries(&fx.workspace));
}

#[tokio::test]
async fn a_second_file_part_is_rejected_and_leaves_no_file() {
    let fx = Fixture::new().await;

    let (status, json) = fx
        .upload(
            Form::default()
                .target(&fx.pe_id, "")
                .file("one.txt", b"one")
                .file("two.txt", b"two"),
        )
        .await;

    assert_bad_request(status, &json, "only one 'file' part is allowed");
    assert!(entries(&fx.workspace).is_empty(), "{:?}", entries(&fx.workspace));
}

#[tokio::test]
async fn a_part_after_the_file_is_rejected_and_leaves_no_file() {
    let fx = Fixture::new().await;

    // `file_name` has to precede `file` as well.
    let (status, json) = fx
        .upload(
            Form::default()
                .target(&fx.pe_id, "")
                .file("late.txt", b"late")
                .text("file_name", "renamed.txt"),
        )
        .await;

    assert_bad_request(status, &json, "'file' must be the last part");
    assert!(entries(&fx.workspace).is_empty(), "{:?}", entries(&fx.workspace));
}

#[tokio::test]
async fn a_target_that_is_not_an_existing_directory_is_rejected() {
    let fx = Fixture::new().await;
    std::fs::write(fx.path("readme.md"), "# keep").unwrap();
    let to = |relative_path: &str| Form::default().target(&fx.pe_id, relative_path).file("a.txt", b"a");

    let (status, json) = fx.upload(to("readme.md")).await;
    assert_bad_request(status, &json, "upload target is not a directory");

    let (status, json) = fx.upload(to("readme.md/inner")).await;
    assert_bad_request(status, &json, "upload target is not a directory");

    let (status, json) = fx.upload(to("missing")).await;
    assert_bad_request(status, &json, "upload target directory does not exist");

    // Lexical escapes are refused by `resolve_reference` itself.
    for escape in ["../outside", "/etc"] {
        let (status, json) = fx.upload(to(escape)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{escape}: {json}");
        assert_eq!(json["code"], "invalid_resource", "{escape}: {json}");
    }

    assert_eq!(std::fs::read_to_string(fx.path("readme.md")).unwrap(), "# keep");
    assert_eq!(entries(&fx.workspace), ["readme.md"]);
    assert_eq!(entries(fx.scratch.path()), ["workspace"]);
}

#[tokio::test]
async fn a_body_over_the_limit_is_rejected_with_413() {
    let fx = Fixture::new().await;
    let (body, len) = streamed_upload(&fx.pe_id, PROJECT_UPLOAD_MAX_SIZE + 2 * MIB);

    let (status, json) = send(&fx.app, upload_request(body, Some(len), fx.auth())).await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{json}");
    assert_eq!(json["success"], false);
    assert_eq!(json["code"], "PAYLOAD_TOO_LARGE");
    assert!(entries(&fx.workspace).is_empty(), "{:?}", entries(&fx.workspace));
}

#[tokio::test]
async fn content_one_byte_over_the_cap_is_rejected_and_removed() {
    let fx = Fixture::new().await;
    let (body, len) = streamed_upload(&fx.pe_id, PROJECT_UPLOAD_MAX_SIZE + 1);
    assert!(
        len < PROJECT_UPLOAD_MAX_SIZE + MIB,
        "the body itself fits under the route limit"
    );

    let (status, json) = send(&fx.app, upload_request(body, Some(len), fx.auth())).await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{json}");
    assert_eq!(json["code"], "PAYLOAD_TOO_LARGE");
    assert!(
        entries(&fx.workspace).is_empty(),
        "the partial file must be removed: {:?}",
        entries(&fx.workspace)
    );
}

#[tokio::test]
async fn a_body_without_content_length_is_cut_off_at_the_limit_with_413() {
    let fx = Fixture::new().await;
    // An oversized part ahead of the upload: the limit trips while the body is
    // being read, not on the declared length.
    let head = text_part_head("padding").into_bytes();
    let mut tail = b"\r\n".to_vec();
    tail.extend(Form::default().target(&fx.pe_id, "").file("a.txt", b"a").build());
    let (body, _) = streamed_body(head, PROJECT_UPLOAD_MAX_SIZE + 2 * MIB, tail);

    let (status, json) = send(&fx.app, upload_request(body, None, fx.auth())).await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{json}");
    assert_eq!(json["code"], "PAYLOAD_TOO_LARGE");
    assert!(entries(&fx.workspace).is_empty(), "{:?}", entries(&fx.workspace));
}

// ===========================================================================
// Security
// ===========================================================================

#[tokio::test]
async fn another_users_entry_is_not_found() {
    let fx = Fixture::new().await;
    let mut app = fx.app.clone();
    let (bob_token, bob_csrf) = setup_and_login(&mut app, &fx.services, "bob", "StrongP@ss2").await;
    let bob = Auth {
        token: Some(&bob_token),
        csrf: Some(&bob_csrf),
        act_as: None,
    };

    let (status, json) = fx
        .upload_as(Form::default().target(&fx.pe_id, "").file("intrusion.txt", b"x"), bob)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    assert_eq!(json["code"], "project_explorer_not_found");
    assert!(
        entries(&fx.workspace).is_empty(),
        "nothing may land in another user's folder"
    );

    // Bob's session is valid: his own entry takes the same upload.
    let bob_dir = fx.scratch.path().join("bob");
    std::fs::create_dir(&bob_dir).unwrap();
    let bob_id = fx.services.user_repo.find_by_username("bob").await.unwrap().unwrap().id;
    let bob_pe_id = register_folder(&fx.services, &bob_id, &bob_dir).await;
    let (status, json) = fx
        .upload_as(Form::default().target(&bob_pe_id, "").file("mine.txt", b"x"), bob)
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(bob_dir.join("mine.txt").is_file());
}

#[tokio::test]
async fn an_unauthenticated_upload_is_rejected_with_401() {
    let fx = Fixture::new().await;
    let form = || Form::default().target(&fx.pe_id, "").file("a.txt", b"a");

    let anonymous = Auth {
        token: None,
        csrf: Some(&fx.csrf),
        act_as: None,
    };
    let (status, json) = fx.upload_as(form(), anonymous).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{json}");
    assert_eq!(json["code"], "UNAUTHORIZED");

    let forged = Auth {
        token: Some("not-a-session-token"),
        csrf: Some(&fx.csrf),
        act_as: None,
    };
    let (status, json) = fx.upload_as(form(), forged).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{json}");
    assert_eq!(json["code"], "UNAUTHORIZED");

    assert!(entries(&fx.workspace).is_empty(), "{:?}", entries(&fx.workspace));
}

#[tokio::test]
async fn an_upload_without_the_csrf_token_is_rejected() {
    let fx = Fixture::new().await;
    let no_csrf = Auth {
        token: Some(&fx.token),
        csrf: None,
        act_as: None,
    };

    let (status, json) = fx
        .upload_as(Form::default().target(&fx.pe_id, "").file("a.txt", b"a"), no_csrf)
        .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(json["code"], "CSRF_INVALID");
    assert!(entries(&fx.workspace).is_empty(), "{:?}", entries(&fx.workspace));
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_leading_out_of_the_entry_is_rejected() {
    let fx = Fixture::new().await;
    let outside = fx.scratch.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, fx.path("link")).unwrap();

    let (status, json) = fx
        .upload(Form::default().target(&fx.pe_id, "link").file("a.txt", b"a"))
        .await;

    assert_bad_request(status, &json, "upload target is outside the project folder");
    assert!(entries(&outside).is_empty(), "nothing may be written through the link");
}

#[tokio::test]
async fn responses_never_carry_the_absolute_path() {
    let fx = Fixture::new().await;
    std::fs::write(fx.path("readme.md"), "x").unwrap();
    let absolute = fx.workspace.to_string_lossy().into_owned();

    let (status, ok) = fx
        .upload(Form::default().target(&fx.pe_id, "").file("a.txt", b"a"))
        .await;
    assert_eq!(status, StatusCode::OK, "{ok}");
    let (status, not_dir) = fx
        .upload(Form::default().target(&fx.pe_id, "readme.md").file("b.txt", b"b"))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{not_dir}");
    let (status, missing) = fx
        .upload(Form::default().target(&fx.pe_id, "gone/deeper").file("c.txt", b"c"))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{missing}");

    for json in [ok, not_dir, missing] {
        assert!(!json.to_string().contains(&absolute), "absolute path leaked: {json}");
    }
}

// ===========================================================================
// Streaming into a private temp
// ===========================================================================

#[tokio::test]
async fn the_final_name_does_not_exist_until_the_upload_completes() {
    let fx = Fixture::new().await;
    let (feed, response) = fx.start_fed_upload();
    feed.send(Ok(upload_head(&fx.pe_id, "", "a.txt"))).unwrap();
    feed.send(Ok(Bytes::from(vec![b'1'; PAST_THE_WRITE_BUFFER]))).unwrap();

    wait_for_streaming(&fx.workspace).await;
    assert!(
        !fx.path("a.txt").exists(),
        "a partial file must never sit under the final name"
    );
    assert_eq!(temps(&fx.workspace).len(), 1, "{:?}", entries(&fx.workspace));

    feed.send(Ok(Bytes::from_static(b"2"))).unwrap();
    feed.send(Ok(upload_tail())).unwrap();
    drop(feed);
    let (status, json) = response.await.unwrap();

    assert_eq!(status, StatusCode::OK, "{json}");
    let mut expected = vec![b'1'; PAST_THE_WRITE_BUFFER];
    expected.push(b'2');
    assert_eq!(std::fs::read(fx.path("a.txt")).unwrap(), expected);
    assert_eq!(entries(&fx.workspace), ["a.txt"], "the temp is gone");
}

#[tokio::test]
async fn a_mid_file_disconnect_leaves_nothing_behind() {
    let fx = Fixture::new().await;
    let (feed, response) = fx.start_fed_upload();
    feed.send(Ok(upload_head(&fx.pe_id, "", "a.txt"))).unwrap();
    feed.send(Ok(Bytes::from(vec![b'1'; PAST_THE_WRITE_BUFFER]))).unwrap();
    wait_for_streaming(&fx.workspace).await;

    feed.send(Err(std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "client went away",
    )))
    .unwrap();
    let (status, json) = response.await.unwrap();

    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["code"], "BAD_REQUEST");
    assert!(
        entries(&fx.workspace).is_empty(),
        "neither the final name nor the temp may remain: {:?}",
        entries(&fx.workspace)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_dangling_symlink_at_the_requested_name_is_renamed_around_and_not_followed() {
    let fx = Fixture::new().await;
    let target = fx.scratch.path().join("missing-target.txt");
    std::os::unix::fs::symlink(&target, fx.path("a.txt")).unwrap();

    let (status, json) = fx
        .upload(Form::default().target(&fx.pe_id, "").file("a.txt", b"x"))
        .await;

    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json["data"],
        json!({ "name": "a copy.txt", "relative_path": "a copy.txt", "size": 1, "renamed": true })
    );
    assert!(!target.exists(), "nothing may be created at the link target");
    assert!(
        std::fs::symlink_metadata(fx.path("a.txt"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read(fx.path("a copy.txt")).unwrap(), b"x");
}

#[tokio::test]
async fn a_long_name_that_collides_is_shortened_instead_of_failing() {
    let fx = Fixture::new().await;
    let long = format!("{}.txt", "n".repeat(251));
    let form = |data: &[u8]| Form::default().target(&fx.pe_id, "").file(&long, data);

    let (status, first) = fx.upload(form(b"first")).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["data"]["name"], long.as_str());

    let (status, second) = fx.upload(form(b"second")).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let name = second["data"]["name"].as_str().unwrap();
    assert_eq!(name, format!("{} copy.txt", "n".repeat(246)));
    assert!(name.len() <= 255);
    assert_eq!(second["data"]["renamed"], true);
    assert_eq!(std::fs::read(fx.path(name)).unwrap(), b"second");
    assert_eq!(std::fs::read(fx.path(&long)).unwrap(), b"first");
}

// ===========================================================================
// Client errors vs server errors
// ===========================================================================

#[tokio::test]
async fn target_paths_no_filesystem_takes_are_rejected_with_400() {
    let fx = Fixture::new().await;
    let to = |relative_path: &str| Form::default().target(&fx.pe_id, relative_path).file("a.txt", b"a");

    let (status, json) = fx.upload(to("sub\0dir")).await;
    assert_bad_request(status, &json, "invalid upload target path");

    let (status, json) = fx.upload(to(&"s".repeat(256))).await;
    assert_bad_request(status, &json, "upload target path is too long");

    let (status, json) = fx.upload(to(&"d/".repeat(2100))).await;
    assert_bad_request(status, &json, "upload target path is too long");

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(fx.path("loop"), fx.path("loop")).unwrap();
        let (status, json) = fx.upload(to("loop")).await;
        assert_bad_request(status, &json, "upload target path has a symlink loop");
    }

    assert!(temps(&fx.workspace).is_empty());
    assert!(!fx.path("a.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_directory_without_write_permission_is_forbidden() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fixture::new().await;
    let locked = fx.path("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root (or CAP_DAC_OVERRIDE) writes anyway: then there is nothing to check.
    let probe = locked.join("probe");
    if std::fs::write(&probe, b"x").is_ok() {
        std::fs::remove_file(&probe).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipped: permissions are not enforced for this user");
        return;
    }

    let (status, json) = fx
        .upload(Form::default().target(&fx.pe_id, "locked").file("a.txt", b"a"))
        .await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(json["code"], "FORBIDDEN");
    assert!(entries(&locked).is_empty());
}

// ===========================================================================
// The /api/fs/list cache
// ===========================================================================

#[tokio::test]
async fn the_listing_shows_an_upload_once_it_lands() {
    let fx = Fixture::new().await;
    assert!(fx.list().await.is_empty());

    let (status, json) = fx
        .upload(Form::default().target(&fx.pe_id, "").file("a.txt", b"a"))
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(fx.list().await, ["a.txt"]);

    // The cached listing stays as it was while an upload streams, and is rebuilt
    // once the file lands (picking up `c.txt`, made meanwhile, as well).
    let (feed, response) = fx.start_fed_upload();
    feed.send(Ok(upload_head(&fx.pe_id, "", "b.txt"))).unwrap();
    feed.send(Ok(Bytes::from(vec![b'b'; PAST_THE_WRITE_BUFFER]))).unwrap();
    wait_for_streaming(&fx.workspace).await;
    std::fs::write(fx.path("c.txt"), "made meanwhile").unwrap();
    let during = fx.list().await;
    assert_eq!(during, ["a.txt"], "starting an upload leaves the cached listing alone");

    feed.send(Ok(upload_tail())).unwrap();
    drop(feed);
    let (status, json) = response.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(fx.list().await, ["a.txt", "b.txt", "c.txt"]);
}

#[tokio::test]
async fn a_listing_taken_mid_upload_gives_way_to_the_landed_file() {
    let fx = Fixture::new().await;
    let (feed, response) = fx.start_fed_upload();
    feed.send(Ok(upload_head(&fx.pe_id, "", "a.txt"))).unwrap();
    feed.send(Ok(Bytes::from(vec![b'a'; PAST_THE_WRITE_BUFFER]))).unwrap();
    wait_for_streaming(&fx.workspace).await;

    // `/api/fs/list` includes dotfiles, so this listing caches the temp.
    let during = fx.list().await;
    assert_eq!(during.len(), 1, "{during:?}");
    assert!(during[0].starts_with(PROJECT_UPLOAD_TEMP_PREFIX), "{during:?}");

    feed.send(Ok(upload_tail())).unwrap();
    drop(feed);
    assert_eq!(response.await.unwrap().0, StatusCode::OK);
    assert_eq!(fx.list().await, ["a.txt"]);
}

#[tokio::test]
async fn a_listing_taken_mid_upload_drops_the_temp_of_a_failed_upload() {
    let fx = Fixture::new().await;
    let (feed, response) = fx.start_fed_upload();
    feed.send(Ok(upload_head(&fx.pe_id, "", "a.txt"))).unwrap();
    feed.send(Ok(Bytes::from(vec![b'a'; PAST_THE_WRITE_BUFFER]))).unwrap();
    wait_for_streaming(&fx.workspace).await;

    let during = fx.list().await;
    assert_eq!(during.len(), 1, "{during:?}");
    assert!(during[0].starts_with(PROJECT_UPLOAD_TEMP_PREFIX), "{during:?}");

    feed.send(Err(std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "client went away",
    )))
    .unwrap();
    assert_eq!(response.await.unwrap().0, StatusCode::BAD_REQUEST);
    assert!(fx.list().await.is_empty(), "the removed temp leaves the listing too");
}

// ===========================================================================
// Load limits
// ===========================================================================

#[tokio::test]
async fn a_stalled_upload_is_abandoned_with_408_and_cleaned_up() {
    let fx = Fixture::with_limits(ProjectUploadLimits {
        idle_timeout: Duration::from_millis(300),
        ..ProjectUploadLimits::default()
    })
    .await;
    let (feed, response) = fx.start_fed_upload();
    feed.send(Ok(upload_head(&fx.pe_id, "", "a.txt"))).unwrap();
    feed.send(Ok(Bytes::from(vec![b'1'; PAST_THE_WRITE_BUFFER]))).unwrap();
    // ...and then nothing, with the connection still open.

    let (status, json) = tokio::time::timeout(Duration::from_secs(10), response)
        .await
        .expect("a stalled upload must be abandoned")
        .unwrap();

    assert_eq!(status, StatusCode::REQUEST_TIMEOUT, "{json}");
    assert_eq!(json["code"], "UPLOAD_STALLED");
    assert!(entries(&fx.workspace).is_empty(), "{:?}", entries(&fx.workspace));
    drop(feed);
}

#[tokio::test]
async fn uploads_beyond_the_slots_wait_for_one_then_get_503() {
    let fx = Fixture::with_limits(ProjectUploadLimits {
        slots: Arc::new(Semaphore::new(1)),
        queue_wait: Duration::from_millis(500),
        idle_timeout: Duration::from_secs(10),
    })
    .await;

    // A holds the only slot.
    let (feed_a, response_a) = fx.start_fed_upload();
    feed_a.send(Ok(upload_head(&fx.pe_id, "", "a.txt"))).unwrap();
    eventually("the first upload to start", || temps(&fx.workspace).len() == 1).await;

    // B waits for the slot and gets it once A is done.
    let b = {
        let app = fx.app.clone();
        let body = Form::default().target(&fx.pe_id, "").file("b.txt", b"b").build();
        let len = body.len();
        let request = upload_request(Body::from(body), Some(len), fx.auth());
        tokio::spawn(async move { send(&app, request).await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!b.is_finished(), "the second upload waits for the slot");
    feed_a.send(Ok(Bytes::from_static(b"a"))).unwrap();
    feed_a.send(Ok(upload_tail())).unwrap();
    drop(feed_a);
    assert_eq!(response_a.await.unwrap().0, StatusCode::OK);
    let (status, json) = b.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{json}");

    // C holds the slot past D's wait: D is turned away.
    let (feed_c, response_c) = fx.start_fed_upload();
    feed_c.send(Ok(upload_head(&fx.pe_id, "", "c.txt"))).unwrap();
    eventually("the third upload to start", || temps(&fx.workspace).len() == 1).await;
    let (status, json) = fx
        .upload(Form::default().target(&fx.pe_id, "").file("d.txt", b"d"))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
    assert_eq!(json["code"], "UPLOADS_BUSY");

    // Ending C's body before the file is complete is a client error that cleans up.
    drop(feed_c);
    assert_eq!(response_c.await.unwrap().0, StatusCode::BAD_REQUEST);
    assert_eq!(entries(&fx.workspace), ["a.txt", "b.txt"]);
}

// ===========================================================================
// Acting as another user
// ===========================================================================

#[tokio::test]
async fn act_as_lands_in_the_target_users_entry_only() {
    let fx = Fixture::new().await;
    let mut app = fx.app.clone();
    setup_and_login(&mut app, &fx.services, "bob", "StrongP@ss2").await;
    let bob_id = fx.services.user_repo.find_by_username("bob").await.unwrap().unwrap().id;
    let bob_dir = fx.scratch.path().join("bob");
    std::fs::create_dir(&bob_dir).unwrap();
    let bob_pe_id = register_folder(&fx.services, &bob_id, &bob_dir).await;
    let as_bob = Auth {
        act_as: Some(&bob_id),
        ..fx.auth()
    };

    let (status, json) = fx
        .upload_as(
            Form::default().target(&bob_pe_id, "").file("from-admin.txt", b"x"),
            as_bob,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(std::fs::read(bob_dir.join("from-admin.txt")).unwrap(), b"x");

    // Acting as Bob, the admin's own entry is out of reach.
    let (status, json) = fx
        .upload_as(Form::default().target(&fx.pe_id, "").file("own.txt", b"x"), as_bob)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    assert_eq!(json["code"], "project_explorer_not_found");
    assert!(entries(&fx.workspace).is_empty(), "{:?}", entries(&fx.workspace));
}
