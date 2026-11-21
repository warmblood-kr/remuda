//! Bounded VFS reads and decode for document browsing.

use super::types::Reply;
use super::{Entry, Location, Page, MAX_BYTES, MAX_LINES};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use remuda_core::{
    protocol::{Request, Response},
    vfs,
};
use serde_json::Value;
use std::{path::Path, time::Duration};

#[allow(dead_code)] // called by the browser layer that follows.
pub(crate) fn fetch(socket: &Path, location: &Location) -> Reply {
    let uri = format!("remuda:///{}", location.path());
    let args = match location {
        Location::Directory(_) => vec!["ls".into(), uri, "--json".into()],
        Location::Document(path) => vec![
            "read".into(),
            uri,
            "--max-bytes".into(),
            if html_path(path) {
                crate::document_html::MAX_HTML_BYTES
            } else {
                MAX_BYTES
            }
            .to_string(),
        ],
    };
    let response = crate::client::request_with_deadline(
        socket,
        &Request::Vfs {
            args,
            input_base64: None,
        },
        Duration::from_secs(3),
    )
    .map_err(|error| error.to_string())?;
    let Response::CommandResult {
        exit_code,
        stdout_base64,
        stderr_base64,
    } = response
    else {
        return Err("Daemon does not support document reads".into());
    };
    if exit_code != 0 {
        return Err(
            String::from_utf8_lossy(&BASE64.decode(stderr_base64).unwrap_or_default()).into_owned(),
        );
    }
    if stdout_base64.len() > 4 * MAX_BYTES {
        return Err("Document response is too large".into());
    }
    let bytes = BASE64
        .decode(stdout_base64)
        .map_err(|_| "Invalid document response")?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "Invalid document JSON")?;
    decode(location, &value)
}

/// Read bounded binary media (an image) from the VFS. The caller decides the
/// format and pixel limits; this function only enforces the byte bound and the
/// same identity/size checks as a text read.
pub fn read_media(socket: &Path, path: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
    vfs::validate_path(path).map_err(|_| "Invalid VFS address")?;
    let uri = format!("remuda:///{path}");
    let response = crate::client::request_with_deadline(
        socket,
        &Request::Vfs {
            args: vec![
                "read".into(),
                uri,
                "--max-bytes".into(),
                max_bytes.to_string(),
            ],
            input_base64: None,
        },
        Duration::from_secs(3),
    )
    .map_err(|error| error.to_string())?;
    let Response::CommandResult {
        exit_code,
        stdout_base64,
        stderr_base64,
    } = response
    else {
        return Err("Daemon does not support document reads".into());
    };
    if exit_code != 0 {
        return Err(
            String::from_utf8_lossy(&BASE64.decode(stderr_base64).unwrap_or_default()).into_owned(),
        );
    }
    // The VFS reply is JSON, its `content` is base64 of the binary, and the
    // whole JSON is base64 again on stdout. Size the outer bound for both
    // encodings plus metadata instead of comparing it to the binary limit.
    let inner_base64 = max_bytes.div_ceil(3).saturating_mul(4);
    let json = inner_base64.saturating_add(64 * 1024);
    let outer_base64 = json.div_ceil(3).saturating_mul(4).saturating_add(64 * 1024);
    if stdout_base64.len() > outer_base64 {
        return Err("Media response is too large".into());
    }
    let bytes = BASE64
        .decode(stdout_base64)
        .map_err(|_| "Invalid media response")?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "Invalid media JSON")?;
    if value["ok"].as_bool() != Some(true) {
        return Err(format!(
            "{}: {}",
            value["code"].as_str().unwrap_or("io"),
            value["detail"].as_str().unwrap_or("Read failed")
        ));
    }
    if value["path"].as_str() != Some(path) {
        return Err("Media identity changed".into());
    }
    let hash = value["sha256"].as_str().ok_or("Missing media hash")?;
    vfs::validate_sha256(hash).map_err(|_| "Invalid media hash")?;
    let content = value["content"].as_str().ok_or("Missing media content")?;
    let decoded = BASE64
        .decode(content)
        .map_err(|_| "Invalid media encoding")?;
    if decoded.len() > max_bytes || value["size"].as_u64() != Some(decoded.len() as u64) {
        return Err("Invalid media size".into());
    }
    Ok(decoded)
}

pub(crate) fn decode(location: &Location, value: &Value) -> Reply {
    let mut page = Page::empty(location.clone());
    match location {
        Location::Directory(prefix) => {
            let rows = value.as_array().ok_or("Invalid directory listing")?;
            if rows.len() > 10_000 {
                return Err("Directory listing is too large".into());
            }
            for row in rows {
                let path = row["path"].as_str().ok_or("Missing VFS path")?;
                let directory = match row["kind"].as_str() {
                    Some("dir") => true,
                    Some("file") => false,
                    _ => return Err("Invalid VFS entry kind".into()),
                };
                vfs::validate_path(path.trim_end_matches('/')).map_err(|_| "Invalid VFS path")?;
                let child = path
                    .strip_prefix(prefix)
                    .ok_or("Entry outside directory")?
                    .trim_end_matches('/');
                if child.is_empty() || child.contains('/') || directory != path.ends_with('/') {
                    return Err("Entry is not an immediate child".into());
                }
                page.entries.push(Entry {
                    path: path.to_owned(),
                    directory,
                    label: None,
                });
            }
        }
        Location::Document(path) => {
            if value["ok"].as_bool() != Some(true) {
                return Err(format!(
                    "{}: {}",
                    value["code"].as_str().unwrap_or("io"),
                    value["detail"].as_str().unwrap_or("Read failed")
                ));
            }
            if value["path"].as_str() != Some(path) {
                return Err("Document identity changed".into());
            }
            let hash = value["sha256"].as_str().ok_or("Missing document hash")?;
            vfs::validate_sha256(hash).map_err(|_| "Invalid document hash")?;
            let version = value["version"]
                .as_u64()
                .ok_or("Missing document version")?;
            let content = value["content"]
                .as_str()
                .ok_or("Missing document content")?;
            if content.len() > MAX_BYTES.div_ceil(3) * 4 {
                return Err("Document exceeds 1 MiB".into());
            }
            let bytes = BASE64
                .decode(content)
                .map_err(|_| "Invalid document encoding")?;
            if bytes.len() > MAX_BYTES || value["size"].as_u64() != Some(bytes.len() as u64) {
                return Err("Invalid document size".into());
            }
            let text = String::from_utf8(bytes).map_err(|_| "Document is not UTF-8 text")?;
            if text.contains('\0') {
                return Err("Document is binary".into());
            }
            page.source_text = Some(text.as_str().into());
            if html_path(path) {
                let html = crate::document_html::parse(path, &text)?;
                page.text = html.lines.clone();
                page.projection = Some(html);
            } else if markdown_path(path) {
                let markdown = crate::document_markdown::parse(path, &text)?;
                page.text = markdown.lines.clone();
                page.projection = Some(markdown);
                page.markdown_source = page.source_text.clone();
            } else {
                page.text = text.lines().take(MAX_LINES).map(str::to_owned).collect();
                if text.lines().count() > MAX_LINES {
                    page.text.push("[… line limit reached]".into());
                }
            }
            page.identity = Some((version, hash.to_owned()));
        }
    }
    Ok(page)
}

pub(crate) fn html_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".html") || path.ends_with(".htm")
}

pub(crate) fn markdown_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".md") || path.ends_with(".markdown")
}
