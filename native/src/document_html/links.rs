//! Local VFS link semantics; no adapter may interpret a blocked target as a URL.

use remuda_core::vfs;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Vfs(String),
    Blocked(String),
}

pub fn resolve(origin: &str, href: &str) -> Target {
    match local_path(origin, href) {
        Ok(path) => Target::Vfs(path),
        Err(reason) => Target::Blocked(reason.into()),
    }
}

fn local_path(origin: &str, href: &str) -> Result<String, &'static str> {
    vfs::validate_path(origin).map_err(|_| "Invalid document address")?;
    let href = decode(href.trim_matches(' '))?;
    if href.chars().any(|ch| ch.is_control() || ch == '\\') {
        return Err("Control characters and backslashes are not allowed in links");
    }
    if href.contains('#') {
        return Err("Fragment links are not supported yet");
    }
    if href.contains('?') {
        return Err("Query links are not supported yet");
    }
    let (path, root) = if let Some(path) = href.strip_prefix("remuda:///") {
        (path, true)
    } else if href.starts_with("//")
        || href
            .split('/')
            .next()
            .is_some_and(|part| part.contains(':'))
    {
        return Err("Only local VFS links are supported");
    } else if let Some(path) = href.strip_prefix('/') {
        (path, true)
    } else {
        (href.as_str(), false)
    };
    if path.is_empty() && !root {
        return Ok(origin.into());
    }
    let directory = path.ends_with('/') || matches!(path.rsplit('/').next(), Some("." | ".."));
    let mut parts: Vec<&str> = if root {
        Vec::new()
    } else {
        origin.split('/').collect()
    };
    if !root {
        parts.pop();
    }
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop().ok_or("Link escapes the VFS root")?;
            }
            other => parts.push(other),
        }
    }
    let mut path = parts.join("/");
    if !path.is_empty() {
        vfs::validate_path(&path).map_err(|_| "Invalid VFS link address")?;
        if directory {
            path.push('/');
        }
    }
    Ok(path)
}

fn decode(text: &str) -> Result<String, &'static str> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes
                .get(index + 1..index + 3)
                .ok_or("Invalid link encoding")?;
            let hex = std::str::from_utf8(hex).map_err(|_| "Invalid link encoding")?;
            decoded.push(u8::from_str_radix(hex, 16).map_err(|_| "Invalid link encoding")?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| "Link address is not UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_root_and_percent_encoded_paths_share_one_vfs_meaning() {
        for (href, path) in [
            ("sub/next.html", "docs/sub/next.html"),
            ("../next.html", "next.html"),
            ("/next.html", "next.html"),
            ("remuda:///next.html", "next.html"),
            ("%E7%95%8C.html", "docs/界.html"),
            ("%2e%2e/next.html", "next.html"),
            ("sub/", "docs/sub/"),
            (".", "docs/"),
            ("..", ""),
            ("", "docs/index.html"),
        ] {
            assert_eq!(resolve("docs/index.html", href), Target::Vfs(path.into()));
        }
    }
    #[test]
    fn unsafe_or_unsupported_links_cannot_silently_become_other_targets() {
        for href in [
            "../../outside",
            "%2e%2e/%2e%2e/outside",
            "javascript:alert(1)",
            "data:text/html,x",
            "file:///x",
            "https://example.com",
            "//host/x",
            "remuda://other/x",
            "next#anchor",
            "#here",
            "next%23anchor",
            "next?query",
            "%00x",
            "a\nb",
            "a%09b",
            "a\\b",
            "%ff",
            "%xz",
        ] {
            assert!(
                matches!(resolve("docs/index.html", href), Target::Blocked(_)),
                "{href}"
            );
        }
    }
}
