use crate::{discover_peers, peer_is_unique, worker_deadline, Selection};
use remuda_native::session_preview::{capture, IdentityPolicy, PreviewFrame};
use std::time::{Duration, Instant};

#[derive(serde::Serialize, serde::Deserialize)]
pub struct SyncFrame {
    pub text: String,
    pub version: u64,
    pub instance: String,
}

fn plain(frame: PreviewFrame) -> Result<String, String> {
    let mut text = String::new();
    for row in frame.cells {
        for cell in row {
            for ch in cell.text.chars() {
                if ch.is_control()
                    || remuda_core::protocol::is_secret_prompt_format_or_separator(ch)
                {
                    continue;
                }
                if ch.len_utf8() > 65_536usize.saturating_sub(text.len()) {
                    return Err("too large".into());
                }
                text.push(ch);
            }
        }
        if text.len() == 65_536 {
            return Err("too large".into());
        }
        text.push('\n');
    }
    Ok(text)
}

fn fetch(s: &Selection) -> Result<String, String> {
    let result = match &s.peer {
        None => capture(
            remuda_native::daemon::socket_path("default").as_path(),
            &s.name,
            Some(&s.instance),
            0,
            IdentityPolicy::Strict,
        )
        .and_then(plain),
        Some(fp) => {
            if !peer_is_unique(&discover_peers().map_err(|e| e.to_string())?, fp) {
                return Err("peer unavailable".into());
            }
            match remuda_native::cluster_api::resolve_call(fp, None).map_err(|e| e.to_string())? {
                remuda_native::cluster_api::ResolvedCall::Remote { fingerprint, .. }
                    if fingerprint == *fp => {}
                _ => return Err("peer unavailable".into()),
            }
            let port = remuda_native::tui::RemotePort::system(fp).map_err(|e| e.to_string())?;
            let deadline = Instant::now() + Duration::from_secs(4);
            let text = loop {
                match capture(&port, &s.name, Some(&s.instance), 0, IdentityPolicy::Strict)
                    .and_then(plain)
                {
                    Ok(text) => break Ok(text),
                    Err(e) if Instant::now() >= deadline => break Err(e),
                    Err(_) => std::thread::sleep(Duration::from_millis(100)),
                }
            };
            if !peer_is_unique(&discover_peers().map_err(|e| e.to_string())?, fp) {
                return Err("peer unavailable".into());
            }
            text
        }
    };
    result
}

pub fn worker(args: &[String], tagged: bool) -> i32 {
    if args.len() != 4 || args[2].is_empty() || args[3].is_empty() {
        return 1;
    }
    let peer = match args[0].as_str() {
        "local" if args[1].is_empty() => None,
        "peer" if !args[1].is_empty() => Some(args[1].clone()),
        _ => return 1,
    };
    worker_deadline();
    let s = Selection {
        peer,
        name: args[2].clone(),
        instance: args[3].clone(),
    };
    match fetch(&s) {
        Ok(text) => super::write_worker_reply(&text, tagged),
        Err(_) => 1,
    }
}

/// A separate bounded worker keeps daemon waits and decoding off the GUI thread.
/// The existing preview command rejects these extended arguments on older builds.
pub fn sync_worker(args: &[String]) -> i32 {
    if args.len() != 6
        || !((args[0] == "local" && args[1].is_empty())
            || (args[0] == "peer" && !args[1].is_empty()))
        || args[2].is_empty()
        || args[3].is_empty()
        || args[4] != "--sync"
    {
        return 1;
    }
    let Ok(since) = args[5].parse::<u64>() else {
        return 1;
    };
    worker_deadline();
    let path = remuda_native::daemon::socket_path("default");
    let request = remuda_core::protocol::Request::Sync {
        name: args[2].clone(),
        instance_id: Some(args[3].clone()),
        since,
        timeout_ms: 750,
    };
    let response = if args[0] == "local" {
        remuda_native::client::request_with_timeout(&path, &request, Duration::from_secs(3))
            .map_err(|_| ())
    } else {
        let fingerprint = &args[1];
        let current = || discover_peers().is_ok_and(|peers| peer_is_unique(&peers, fingerprint));
        if !current() {
            return 1;
        }
        let (address, pinned_static_key) =
            match remuda_native::cluster_api::resolve_call(fingerprint, None) {
                Ok(remuda_native::cluster_api::ResolvedCall::Remote {
                    fingerprint: resolved,
                    address,
                    pinned_static_key,
                }) if resolved == *fingerprint => (address, pinned_static_key),
                _ => return 1,
            };
        let private = match remuda_native::cluster::identity::load_static_private_key() {
            Ok(key) => key,
            Err(_) => return 1,
        };
        let response = remuda_native::net::cluster_client::ClusterClient::system()
            .request(address, &pinned_static_key, &private, &request)
            .map_err(|_| ());
        if !current() {
            return 1;
        }
        response
    };
    let result = response
        .and_then(|r| remuda_native::session_preview::decode_sync(r, &args[3]).map_err(|_| ()))
        .and_then(|f| {
            let version = f.output_version.ok_or(())?;
            if version < since {
                return Err(());
            }
            let instance = f.instance_id.clone().ok_or(())?;
            plain(f)
                .map(|text| SyncFrame {
                    text,
                    version,
                    instance,
                })
                .map_err(|_| ())
        });
    match result {
        Ok(frame) => super::write_worker_reply(&frame, true),
        Err(()) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_core::agent::{Cursor, StyledCell};
    #[test]
    fn extended_sync_arguments_are_not_accepted_as_legacy_preview() {
        let args = ["local", "", "session", "instance", "--sync", "0", "abi"].map(str::to_string);
        assert_eq!(worker(&args, false), 1);
    }
    #[test]
    fn text_removes_controls_and_directional_formatting() {
        let frame = PreviewFrame {
            cells: vec![vec![StyledCell {
                text: "a\u{1b}\u{202e}b".into(),
                ..Default::default()
            }]],
            wrapped: vec![],
            cursor: Cursor {
                row: 0,
                col: 0,
                visible: false,
            },
            scrollback_len: 0,
            scrollback_total: 0,
            instance_id: Some("1".into()),
            output_version: None,
        };
        assert_eq!(plain(frame).unwrap(), "ab\n");
    }
}
