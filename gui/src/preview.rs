use crate::{discover_peers, peer_is_unique, worker_deadline, Selection};
use remuda_native::session_preview::{capture, IdentityPolicy, PreviewFrame};
use std::time::{Duration, Instant};

fn styled(frame: PreviewFrame) -> Result<crate::terminal_view::Screen, String> {
    let mut screen = crate::terminal_view::Screen::from_cells(frame.cells)?;
    screen.scrollback_len = frame.scrollback_len;
    Ok(screen)
}

fn fetch(s: &Selection, scroll: usize) -> Result<crate::terminal_view::Screen, String> {
    let result = match &s.peer {
        None => capture(
            remuda_native::daemon::socket_path("default").as_path(),
            &s.name,
            Some(&s.instance),
            scroll,
            IdentityPolicy::Strict,
        )
        .and_then(styled),
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
                match capture(
                    &port,
                    &s.name,
                    Some(&s.instance),
                    scroll,
                    IdentityPolicy::Strict,
                )
                .and_then(styled)
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

pub fn worker(args: &[String], abi: Option<&str>) -> i32 {
    if !matches!(args.len(), 4..=6) || args[2].is_empty() || args[3].is_empty() {
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
    let resize_refused = args
        .get(4)
        .filter(|value| !value.is_empty())
        .is_some_and(|value| !resize(&s, value));
    let scroll = match args.get(5).map(|value| value.parse::<usize>()) {
        Some(Ok(scroll)) => scroll.min(10_000),
        None => 0,
        Some(Err(_)) => return 1,
    };
    match fetch(&s, scroll) {
        Ok(mut screen) if abi == Some(super::PREVIEW_ABI) => {
            screen.resize_refused = resize_refused;
            super::write_worker_reply_with_abi(&screen, abi)
        }
        Ok(screen) => super::write_worker_reply_with_abi(&screen.text, abi),
        Err(_) => 1,
    }
}

fn resize(selection: &Selection, dimensions: &str) -> bool {
    use remuda_core::{
        agent::Size,
        protocol::{Request, Response},
    };
    use remuda_native::strict_input::{InputTransport, LocalInput, PeerInput};

    let Some(size) = dimensions
        .split_once('x')
        .and_then(|(cols, rows)| Some(Size::for_viewport(cols.parse().ok()?, rows.parse().ok()?)))
    else {
        return false;
    };
    let request = Request::ResizeSession {
        name: selection.name.clone(),
        instance_id: selection.instance.clone(),
        size,
    };
    let response = match &selection.peer {
        Some(peer) => PeerInput::new(peer.clone()).and_then(|mut peer| peer.send(&request)),
        None => LocalInput(remuda_native::daemon::socket_path("default")).send(&request),
    };
    matches!(response, Ok(Response::Ok))
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_core::agent::{Color, Cursor, StyledCell};
    #[test]
    fn text_removes_controls_and_directional_formatting() {
        let frame = PreviewFrame {
            cells: vec![vec![StyledCell {
                text: "a\u{1b}\u{202e}b".into(),
                fg: Color::Idx(2),
                bg: Color::Rgb(0, 0, 32),
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
        let screen = styled(frame).unwrap();
        assert_eq!(screen.text, "ab\n");
        assert_eq!(screen.rows[0][0].fg, Color::Idx(2));
        assert_eq!(screen.rows[0][0].bg, Color::Rgb(0, 0, 32));
    }
}
