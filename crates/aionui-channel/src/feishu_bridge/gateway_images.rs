//! Bounded image staging. Remote filenames and paths never select local paths.
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::ChannelError;
use crate::gateway::{GatewayDelivery, GatewayResource, MAX_RESOURCE_BYTES};

fn failed(code: &'static str) -> ChannelError {
    ChannelError::MessageSendFailed(code.into())
}

fn image_extension(resource: &GatewayResource) -> Result<&'static str, ChannelError> {
    let bytes = &resource.bytes;
    if bytes.len() > MAX_RESOURCE_BYTES {
        return Err(failed("GATEWAY_RESOURCE_TOO_LARGE"));
    }
    let (extension, mime) = if bytes.len() >= 24 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") && &bytes[12..16] == b"IHDR"
    {
        ("png", "image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        ("jpg", "image/jpeg")
    } else if bytes.len() >= 10 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        ("gif", "image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        ("webp", "image/webp")
    } else if bytes.len() >= 26 && bytes.starts_with(b"BM") {
        ("bmp", "image/bmp")
    } else {
        return Err(failed("GATEWAY_IMAGE_TYPE_INVALID"));
    };
    let declared = resource.content_type.split(';').next().unwrap_or("").trim();
    if !declared.eq_ignore_ascii_case(mime) {
        return Err(failed("GATEWAY_IMAGE_TYPE_INVALID"));
    }
    Ok(extension)
}

fn hash(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

pub(super) async fn stage_images(
    owner: &str,
    delivery: &GatewayDelivery,
    resources: Vec<GatewayResource>,
) -> Result<Vec<String>, ChannelError> {
    if resources.is_empty() {
        return Ok(Vec::new());
    }
    let owner = owner.to_owned();
    let delivery = delivery.clone();
    tokio::task::spawn_blocking(move || store_images(&std::env::temp_dir(), &owner, &delivery, &resources))
        .await
        .map_err(|_| failed("GATEWAY_IMAGE_STORE_FAILED"))?
}

fn store_images(
    root: &Path,
    owner: &str,
    delivery: &GatewayDelivery,
    resources: &[GatewayResource],
) -> Result<Vec<String>, ChannelError> {
    let namespace = aionui_common::user_upload_dir_name(owner).map_err(|_| failed("GATEWAY_IMAGE_OWNER_INVALID"))?;
    let event = delivery
        .event
        .as_ref()
        .ok_or_else(|| failed("GATEWAY_IMAGE_EVENT_INVALID"))?;
    if resources.len() != event.attachments.len() || resources.len() > 10 {
        return Err(failed("GATEWAY_IMAGE_COUNT_INVALID"));
    }
    // Validate the whole message before creating any files; never stage a partial image set.
    let extensions = resources.iter().map(image_extension).collect::<Result<Vec<_>, _>>()?;
    let directory = format!(
        "feishu-{}",
        hash(&[
            owner,
            &delivery.route.id,
            &delivery.route.generation,
            &event.app_id,
            &event.id
        ])
    );
    let names = event
        .attachments
        .iter()
        .enumerate()
        .zip(extensions)
        .map(|((index, attachment), extension)| {
            let prefix = hash(&[&index.to_string(), &attachment.file_key, &attachment.message_id]);
            (prefix, extension)
        })
        .collect::<Vec<_>>();
    install_images(root, &namespace, &directory, &names, resources)
}

#[cfg(unix)]
fn install_images(
    root: &Path,
    namespace: &str,
    directory: &str,
    names: &[(String, &str)],
    resources: &[GatewayResource],
) -> Result<Vec<String>, ChannelError> {
    use rustix::fs::{AtFlags, FlockOperation, Mode, OFlags, flock, linkat, mkdirat, open, openat, unlinkat};
    use rustix::io::Errno;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::unix::fs::MetadataExt;

    let io_error = |_| failed("GATEWAY_IMAGE_STORE_FAILED");
    // Resolve the trusted OS temp directory once (e.g. macOS /var -> /private/var).
    // Every untrusted descendant is then opened relative to pinned directory FDs.
    let root = root.canonicalize().map_err(|_| failed("GATEWAY_IMAGE_STORE_FAILED"))?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut dir = open(&root, flags, Mode::empty()).map_err(io_error)?;
    let mut path = root;
    for component in ["aionui", namespace, directory] {
        match mkdirat(&dir, component, Mode::RWXU) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(_) => return Err(failed("GATEWAY_IMAGE_STORE_FAILED")),
        }
        dir = openat(&dir, component, flags, Mode::empty()).map_err(io_error)?;
        path.push(component);
    }
    // Nonblocking directory locking serializes retries without adding lock files.
    flock(&dir, FlockOperation::NonBlockingLockExclusive).map_err(io_error)?;
    let mut paths = Vec::new();
    for ((prefix, extension), resource) in names.iter().zip(resources) {
        let filename = format!("{prefix}.{extension}");
        let temporary = format!("{prefix}.part");
        // Remove a crash remnant before inspecting nlink on a committed file.
        match unlinkat(&dir, &temporary, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(_) => return Err(failed("GATEWAY_IMAGE_STORE_FAILED")),
        }
        for candidate_extension in ["png", "jpg", "gif", "webp", "bmp"] {
            let candidate = format!("{prefix}.{candidate_extension}");
            let existing = match openat(
                &dir,
                &candidate,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => Some(File::from(fd)),
                Err(Errno::NOENT) => None,
                Err(_) => return Err(failed("GATEWAY_IMAGE_STORE_FAILED")),
            };
            if let Some(file) = existing {
                let metadata = file.metadata().map_err(|_| failed("GATEWAY_IMAGE_STORE_FAILED"))?;
                if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MAX_RESOURCE_BYTES as u64 {
                    return Err(failed("GATEWAY_IMAGE_STORE_FAILED"));
                }
                let mut bytes = Vec::new();
                file.take(MAX_RESOURCE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| failed("GATEWAY_IMAGE_STORE_FAILED"))?;
                if candidate != filename || bytes != resource.bytes {
                    return Err(failed("GATEWAY_IMAGE_CACHE_CONFLICT"));
                }
            }
        }
        // An existing committed image is verified above and reused as-is.
        // New images use exclusive creation and mode 0600.
        match openat(
            &dir,
            &filename,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(_) => {}
            Err(Errno::NOENT) => {
                let fd = openat(
                    &dir,
                    &temporary,
                    OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::RUSR | Mode::WUSR,
                )
                .map_err(io_error)?;
                let result = (|| {
                    let mut file = File::from(fd);
                    file.write_all(&resource.bytes)
                        .map_err(|_| failed("GATEWAY_IMAGE_STORE_FAILED"))?;
                    file.sync_all().map_err(|_| failed("GATEWAY_IMAGE_STORE_FAILED"))?;
                    // Atomic publish without replacement, unlike rename().
                    linkat(&dir, &temporary, &dir, &filename, AtFlags::empty()).map_err(io_error)?;
                    Ok::<_, ChannelError>(())
                })();
                let cleanup = unlinkat(&dir, &temporary, AtFlags::empty());
                result?;
                cleanup.map_err(io_error)?;
                rustix::fs::fsync(&dir).map_err(io_error)?;
            }
            Err(_) => return Err(failed("GATEWAY_IMAGE_STORE_FAILED")),
        }
        let file_path = path.join(filename);
        if file_path
            .canonicalize()
            .map_err(|_| failed("GATEWAY_IMAGE_STORE_FAILED"))?
            != file_path
        {
            return Err(failed("GATEWAY_IMAGE_STORE_FAILED"));
        }
        paths.push(
            file_path
                .to_str()
                .ok_or_else(|| failed("GATEWAY_IMAGE_STORE_FAILED"))?
                .to_owned(),
        );
    }
    Ok(paths)
}

#[cfg(not(unix))]
fn install_images(
    _: &Path,
    _: &str,
    _: &str,
    _: &[(String, &str)],
    _: &[GatewayResource],
) -> Result<Vec<String>, ChannelError> {
    Err(failed("GATEWAY_IMAGE_SECURE_STORAGE_UNSUPPORTED"))
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("gateway_image_store_tests.rs");
}
