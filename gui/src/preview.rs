use crate::{discover_peers, peer_is_unique, worker_deadline, Selection};
use remuda_native::session_preview::{capture, IdentityPolicy, PreviewFrame};
use std::time::{Duration, Instant};

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

pub fn worker(args: &[String]) -> i32 {
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
        Ok(text) => i32::from(serde_json::to_writer(std::io::stdout(), &text).is_err()),
        Err(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_core::agent::{Cursor, StyledCell};
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
