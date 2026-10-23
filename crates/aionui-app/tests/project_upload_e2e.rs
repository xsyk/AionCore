//! E2E tests for `POST /api/fs/upload-to-project` — uploading one file into a
//! directory of a project entry (the Explorer's "upload file").

mod common;

use std::path::{Path, PathBuf};

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use aionui_app::AppServices;
use aionui_common::constants::PROJECT_UPLOAD_MAX_SIZE;
use common::{body_json, build_app, setup_and_login};

const URI: &str = "/api/fs/upload-to-project";
const BOUNDARY: &str = "----AionProjectUploadBoundary7QpL";
const MIB: usize = 1024 * 1024;

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
    let mut head = Form::default().target(pe_id, "").body;
    head.extend_from_slice(file_part_head("; filename=\"big.bin\"").as_bytes());
    let tail = format!("\r\n--{BOUNDARY}--\r\n").into_bytes();
    streamed_body(head, file_len, tail)
}

/// Credentials attached to a request; `None` leaves that header off.
#[derive(Clone, Copy)]
struct Auth<'a> {
    token: Option<&'a str>,
    csrf: Option<&'a str>,
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
        let (mut app, services) = build_app().await;
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

    fn path(&self, relative: &str) -> PathBuf {
        self.workspace.join(relative)
    }
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
    };
    let (status, json) = fx.upload_as(form(), anonymous).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{json}");
    assert_eq!(json["code"], "UNAUTHORIZED");

    let forged = Auth {
        token: Some("not-a-session-token"),
        csrf: Some(&fx.csrf),
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
