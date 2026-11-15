use std::path::PathBuf;

fn root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("feishu-store-test-{}", aionui_common::generate_id()));
    std::fs::create_dir(&root).unwrap();
    root
}

fn delivery() -> GatewayDelivery {
    GatewayDelivery {
        route: crate::gateway::GatewayRoute {
            id: "route".into(),
            generation: "generation".into(),
            ..Default::default()
        },
        event: Some(crate::gateway::GatewayEvent {
            id: "../../event".into(),
            attachments: vec![crate::gateway::GatewayAttachment {
                kind: "image".into(),
                file_key: "img_key".into(),
                message_id: "om_image".into(),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn png() -> GatewayResource {
    GatewayResource {
        content_type: "image/png".into(),
        bytes: b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR\0\0\0\x01\0\0\0\x01".to_vec(),
    }
}

#[test]
fn byte_signatures_determine_extension_and_reject_mime_spoofing_and_oversize() {
    let root = root();
    let mut resource = png();
    resource.content_type = "image/jpeg".into();
    assert!(
        store_images(&root, "user_owner", &delivery(), &[resource])
            .unwrap_err()
            .to_string()
            .contains("GATEWAY_IMAGE_TYPE_INVALID")
    );
    let resource = GatewayResource {
        content_type: "image/png".into(),
        bytes: b"<html>fake</html>".to_vec(),
    };
    assert!(
        store_images(&root, "user_owner", &delivery(), &[resource])
            .unwrap_err()
            .to_string()
            .contains("GATEWAY_IMAGE_TYPE_INVALID")
    );
    let mut resource = png();
    resource.bytes.resize(MAX_RESOURCE_BYTES + 1, 0);
    assert!(
        store_images(&root, "user_owner", &delivery(), &[resource])
            .unwrap_err()
            .to_string()
            .contains("GATEWAY_RESOURCE_TOO_LARGE")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn retries_reuse_identical_paths_without_overwriting_existing_images() {
    let root = root();
    let first = store_images(&root, "user_owner", &delivery(), &[png()]).unwrap();
    assert_eq!(first, store_images(&root, "user_owner", &delivery(), &[png()]).unwrap());
    assert!(first[0].contains("owner_owner"));
    assert!(!first[0].contains("../"));
    let mut changed = png();
    changed.bytes.push(1);
    assert!(
        store_images(&root, "user_owner", &delivery(), &[changed])
            .unwrap_err()
            .to_string()
            .contains("GATEWAY_IMAGE_CACHE_CONFLICT")
    );
    assert_eq!(std::fs::read(&first[0]).unwrap(), png().bytes);
    let parent = Path::new(&first[0]).parent().unwrap();
    assert_eq!(
        std::fs::read_dir(parent).unwrap().count(),
        1,
        "retry left temporary files"
    );
    assert!(
        store_images(&root, "../other", &delivery(), &[png()])
            .unwrap_err()
            .to_string()
            .contains("GATEWAY_IMAGE_OWNER_INVALID")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn retry_recovers_an_interrupted_atomic_publish_without_extra_files() {
    let root = root();
    let files = store_images(&root, "user_owner", &delivery(), &[png()]).unwrap();
    let final_path = Path::new(&files[0]);
    let temporary = final_path.with_extension("part");
    std::fs::hard_link(final_path, &temporary).unwrap();
    assert_eq!(store_images(&root, "user_owner", &delivery(), &[png()]).unwrap(), files);
    assert!(!temporary.exists());
    std::fs::remove_file(final_path).unwrap();
    std::fs::write(&temporary, b"interrupted partial write").unwrap();
    assert_eq!(store_images(&root, "user_owner", &delivery(), &[png()]).unwrap(), files);
    assert_eq!(std::fs::read(final_path).unwrap(), png().bytes);
    assert_eq!(std::fs::read_dir(final_path.parent().unwrap()).unwrap().count(), 1);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn symlinks_in_owner_directory_or_final_filename_cannot_escape_staging() {
    use std::os::unix::fs::symlink;
    let root = root();
    let outside = root.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::create_dir(root.join("aionui")).unwrap();
    symlink(&outside, root.join("aionui/owner_owner")).unwrap();
    assert!(
        store_images(&root, "user_owner", &delivery(), &[png()])
            .unwrap_err()
            .to_string()
            .contains("GATEWAY_IMAGE_STORE_FAILED")
    );
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    std::fs::remove_file(root.join("aionui/owner_owner")).unwrap();
    let files = store_images(&root, "user_owner", &delivery(), &[png()]).unwrap();
    std::fs::remove_file(&files[0]).unwrap();
    let victim = outside.join("victim");
    std::fs::write(&victim, b"do not overwrite").unwrap();
    symlink(&victim, &files[0]).unwrap();
    assert!(
        store_images(&root, "user_owner", &delivery(), &[png()])
            .unwrap_err()
            .to_string()
            .contains("GATEWAY_IMAGE_STORE_FAILED")
    );
    assert_eq!(std::fs::read(victim).unwrap(), b"do not overwrite");
    std::fs::remove_dir_all(root).unwrap();
}
