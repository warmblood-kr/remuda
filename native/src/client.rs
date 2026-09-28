//! Talking to a daemon, including handing your terminal over to one.

use crate::ipc::{self, Stream, TryClone};
use remuda_core::protocol::{Request, Response};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Detach key: Ctrl-\ (0x1C). Chosen because almost nothing binds it, unlike
/// Ctrl-C/D/Z, which the attached program needs. Consumed, never forwarded.
pub const DETACH: u8 = 0x1C;

const RESET_INPUT_MODES: &[u8] =
    b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?2004l";

fn reset_input_modes(output: &mut impl Write) -> std::io::Result<()> {
    output.write_all(RESET_INPUT_MODES)
}

fn write_input_trace(output: &mut impl Write, at: SystemTime, bytes: &[u8]) -> std::io::Result<()> {
    let elapsed = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    write!(
        output,
        "{}.{:09} ",
        elapsed.as_secs(),
        elapsed.subsec_nanos()
    )?;
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            output.write_all(b" ")?;
        }
        write!(output, "{byte:02x}")?;
    }
    output.write_all(b"\n")
}

fn trace_input_read(path: Option<&Path>, bytes: &[u8]) {
    let Some(path) = path else { return };
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let Ok(mut output) = options.open(path) else {
        return;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(metadata) = output.metadata() else {
            return;
        };
        if metadata.permissions().mode() & 0o077 != 0
            && output
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .is_err()
        {
            return;
        }
    }
    let _ = write_input_trace(&mut output, SystemTime::now(), bytes);
}

/// Send one request and read one response.
pub fn request(path: &Path, request: &Request) -> std::io::Result<Response> {
    let stream = ipc::connect(path)?;
    send(&stream, request)?;
    read_response(&stream)
}

fn send(mut stream: &Stream, request: &Request) -> std::io::Result<()> {
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

fn read_response(stream: &Stream) -> std::io::Result<Response> {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(interpret(&line))
}

/// What a daemon says when it cannot READ what we sent. Only the deserializer
/// produces it — every refusal about content ("no such session") is worded by a
/// handler — so this prefix means one thing and nothing else.
const CANNOT_READ: &str = "bad request: ";

/// One reply, with the two shapes of version skew turned into words a person
/// can act on. Everything else is passed through exactly as the daemon wrote it:
/// a malformed request that is NOT skew must still say what it was.
fn interpret(line: &str) -> Response {
    if line.trim().is_empty() {
        return Response::error("the daemon hung up without answering");
    }
    match serde_json::from_str::<Response>(line) {
        // We serialized that request from this binary's own `Request`, so a
        // daemon on this build CANNOT fail to read it. One that did is not.
        Ok(Response::Error(reason)) => match reason.strip_prefix(CANNOT_READ) {
            Some(detail) => {
                Response::error(skew(&format!("it could not read the request: {detail}")))
            }
            None => Response::Error(reason),
        },
        Ok(other) => other,
        // The mirror: a reply we cannot read, from a daemon newer than we are.
        Err(e) => Response::error(skew(&format!("its reply did not parse: {e}"))),
    }
}

/// Read one protocol line without buffering beyond it. The attach client keeps
/// this exact Windows pipe handle for the output pump so its detach wake can
/// cancel the pending read on the same handle.
fn read_protocol_line(stream: &mut impl Read) -> std::io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while stream.read(&mut byte)? != 0 {
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    String::from_utf8(line)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Every daemon deployed before any handshake existed can never announce its own
/// version, so the client's reaction to the failure is the only diagnosis that
/// reaches those users. See [`tests::the_cure_survives_an_eighty_column_crop`].
fn skew(detail: &str) -> String {
    // One line, cure first. The TUI footer is one row of `cols` and crops the
    // tail: measured at 80 columns, a message that led with the diagnosis lost
    // `remuda stop` off the right edge — the only actionable half of it.
    format!(
        "the daemon is not this build — run `remuda stop`, then this again. \
         This command is {}; the daemon: {detail}",
        crate::dist::BUILD_VERSION,
    )
}

/// Which way a ride ended. Both used to return `Ok(())`, and the terminal
/// looked identical either way — that is the incident this exists for: a second
/// `exit` went to the user's real login shell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Left {
    Detached,
    Exited,
    TakenOver,
}

/// Give this terminal to a session until the user presses [`DETACH`] or the
/// session ends. Leaving does not disturb the session: the process keeps
/// running and this direct attach never changes its size.
pub fn attach(path: &Path, name: &str) -> std::io::Result<Left> {
    attach_with_mouse(path, name, true)
}

enum AttachAck {
    Tracked(u64),
    Legacy,
    Unsupported,
    Refused(String),
}

fn attach_ack(line: &str) -> AttachAck {
    match interpret(line) {
        Response::AttachStarted { generation } => AttachAck::Tracked(generation),
        Response::Ok => AttachAck::Legacy,
        Response::Error(reason) if reason.starts_with("the daemon is not this build") => {
            AttachAck::Unsupported
        }
        Response::Error(reason) => AttachAck::Refused(reason),
        _ => AttachAck::Refused("daemon did not acknowledge attach".into()),
    }
}

fn begin_attach(path: &Path, name: &str) -> std::io::Result<(Stream, Option<u64>)> {
    let mut stream = ipc::connect(path)?;
    send(&stream, &Request::AttachTracked { name: name.into() })?;
    match attach_ack(&read_protocol_line(&mut stream)?) {
        AttachAck::Tracked(generation) => Ok((stream, Some(generation))),
        AttachAck::Legacy => Ok((stream, None)),
        AttachAck::Unsupported => {
            drop(stream);
            let mut stream = ipc::connect(path)?;
            send(&stream, &Request::Attach { name: name.into() })?;
            match interpret(&read_protocol_line(&mut stream)?) {
                Response::Ok => Ok((stream, None)),
                Response::Error(reason) => Err(std::io::Error::other(reason)),
                _ => Err(std::io::Error::other("daemon did not acknowledge attach")),
            }
        }
        AttachAck::Refused(reason) => Err(std::io::Error::other(reason)),
    }
}

fn was_taken_over(path: &Path, name: &str, generation: Option<u64>) -> bool {
    generation.is_some_and(|generation| {
        matches!(
            request(
                path,
                &Request::AttachStatus {
                    name: name.into(),
                    generation
                }
            ),
            Ok(Response::AttachStatus { taken_over: true })
        )
    })
}

// This lifecycle is intentionally linear: attach, start the input reader,
// drain output, wake the reader and restore the terminal in one scope.
#[allow(clippy::too_many_lines)]
pub fn attach_with_mouse(path: &Path, name: &str, mouse: bool) -> std::io::Result<Left> {
    let (stream, generation) = begin_attach(path, name)?;

    let _raw = RawMode::enable()?;
    if mouse {
        let mut stdout = std::io::stdout();
        stdout.write_all(b"\x1b[?1000h\x1b[?1006h")?;
        stdout.flush()?;
    }

    // The Windows wake must cancel the same HANDLE that owns the blocked read;
    // cloning a named-pipe stream creates a different HANDLE. Share this one
    // between the output pump and the key thread that may need to wake it.
    let reader_stream = std::sync::Arc::new(stream);

    // Only the key thread can tell the two exits apart: the reader below just
    // sees the stream end, which is true of a detach and of a death alike.
    let detached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let output_taken_over = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let output_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let output_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let trace_input = std::env::var_os("REMUDA_TRACE_INPUT").map(PathBuf::from);
    let trace_attach_exit = std::env::var_os("REMUDA_TRACE_ATTACH_EXIT").is_some();
    let scrollback = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let output_lock = std::sync::Arc::new(std::sync::Mutex::new(()));
    let attach_path = path.to_path_buf();
    let attach_name = name.to_string();

    // Keystrokes out, on their own thread; the screen pump runs here.
    let keys = std::thread::spawn({
        let mut stream = reader_stream.as_ref().try_clone()?;
        let reader_stream = std::sync::Arc::clone(&reader_stream);
        let detached = std::sync::Arc::clone(&detached);
        let output_taken_over = std::sync::Arc::clone(&output_taken_over);
        let output_stop = std::sync::Arc::clone(&output_stop);
        let output_done = std::sync::Arc::clone(&output_done);
        let scrollback = std::sync::Arc::clone(&scrollback);
        let output_lock = std::sync::Arc::clone(&output_lock);
        let path = attach_path.clone();
        let name = attach_name.clone();
        move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 1024];
            let mut parser = crate::mouse::SgrParser::default();
            let mut mouse_on = mouse;
            let mut logged_pending_read = false;
            loop {
                if output_done.load(std::sync::atomic::Ordering::SeqCst) {
                    if output_taken_over.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    // Keep the advertised "press any key" behavior after the
                    // child exits, but let that key release the attach and
                    // restore terminal modes instead of routing it to nowhere.
                    #[cfg(unix)]
                    {
                        let post_exit_read = read_stdin_timeout(
                            &mut stdin,
                            &mut buf,
                            std::time::Duration::from_secs(86_400),
                        );
                        match post_exit_read {
                            Ok(Some(_)) => {
                                if trace_attach_exit {
                                    eprintln!("attach input trace: post-exit forwarder read a key");
                                }
                                break;
                            }
                            Ok(None) => continue,
                            Err(error) => {
                                if trace_attach_exit {
                                    eprintln!("attach input trace: post-exit read error: {error}");
                                }
                                continue;
                            }
                        }
                    }
                    #[cfg(windows)]
                    {
                        if trace_attach_exit {
                            eprintln!("attach input trace: post-exit ReadConsoleInputW entered");
                        }
                        // Once output has ended, wait directly on the console
                        // input queue. ConPTY keys do not reliably wake the
                        // handle through WaitForSingleObject.
                        let result = wait_for_windows_keypress();
                        if trace_attach_exit {
                            eprintln!("attach input trace: post-exit ReadConsoleInputW returned: {result:?}");
                        }
                        break;
                    }
                }
                let wait = parser
                    .timeout_remaining()
                    .unwrap_or(std::time::Duration::from_millis(25))
                    .min(std::time::Duration::from_millis(25));
                if trace_attach_exit && !logged_pending_read {
                    eprintln!(
                        "attach input trace: forwarder entering stdin read; output_done={}",
                        output_done.load(std::sync::atomic::Ordering::SeqCst)
                    );
                    logged_pending_read = true;
                }
                let read = read_stdin_timeout(&mut stdin, &mut buf, wait);
                if trace_attach_exit {
                    match &read {
                        Ok(Some(n)) => {
                            eprintln!(
                                "attach input trace: forwarder read {n} bytes; output_done={}",
                                output_done.load(std::sync::atomic::Ordering::SeqCst)
                            );
                            logged_pending_read = false;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            eprintln!("attach input trace: forwarder read error: {error}");
                            logged_pending_read = false;
                        }
                    }
                }
                let n = match read {
                    Ok(Some(n)) => n,
                    Ok(None) => {
                        route_tokens(
                            &path,
                            &name,
                            &mut stream,
                            parser.flush_expired(),
                            &mut mouse_on,
                            &scrollback,
                            &output_lock,
                        );
                        continue;
                    }
                    Err(_) => break,
                };
                if output_done.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                if n == 0 {
                    break;
                }
                trace_input_read(trace_input.as_deref(), &buf[..n]);
                match buf[..n].iter().position(|&b| b == DETACH) {
                    // Forward what was typed before the detach key, then stop.
                    // Dropping those bytes would silently swallow input the
                    // user believes they sent.
                    Some(at) => {
                        if mouse {
                            route_tokens(
                                &path,
                                &name,
                                &mut stream,
                                parser.feed(&buf[..at]),
                                &mut mouse_on,
                                &scrollback,
                                &output_lock,
                            );
                        } else if at > 0 {
                            let _ = stream.write_all(&buf[..at]);
                            let _ = stream.flush();
                        }
                        route_tokens(
                            &path,
                            &name,
                            &mut stream,
                            parser.finish(),
                            &mut mouse_on,
                            &scrollback,
                            &output_lock,
                        );
                        detached.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                    None => {
                        if mouse {
                            route_tokens(
                                &path,
                                &name,
                                &mut stream,
                                parser.feed(&buf[..n]),
                                &mut mouse_on,
                                &scrollback,
                                &output_lock,
                            );
                        } else {
                            let _ = stream.write_all(&buf[..n]);
                            let _ = stream.flush();
                        }
                    }
                }
            }
            // Ends the screen pump below, which then returns from `attach` and
            // drops every handle on this connection — that hang-up is what the
            // daemon reads as "the human left".
            ipc::stop_reader(&reader_stream, &output_stop, || {
                output_done.load(std::sync::atomic::Ordering::SeqCst)
            });
        }
    });

    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 8192];
    let mut reader = reader_stream.as_ref();
    while !output_stop.load(std::sync::atomic::Ordering::SeqCst) {
        let Ok(n) = reader.read(&mut buf) else {
            break;
        };
        if n == 0 {
            break;
        }
        let _guard = output_lock.lock().unwrap_or_else(|e| e.into_inner());
        if scrollback.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            continue;
        }
        if stdout.write_all(&buf[..n]).is_err() || stdout.flush().is_err() {
            break;
        }
    }

    let taken_over = was_taken_over(path, name, generation);
    output_taken_over.store(taken_over, std::sync::atomic::Ordering::SeqCst);
    output_done.store(true, std::sync::atomic::Ordering::SeqCst);
    ipc::stop_reader(&reader_stream, &output_stop, || {
        output_done.load(std::sync::atomic::Ordering::SeqCst)
    });
    let left = if taken_over {
        Left::TakenOver
    } else if detached.load(std::sync::atomic::Ordering::SeqCst) {
        Left::Detached
    } else {
        // The key thread remains alive until one key releases the user's
        // terminal after the session exits.
        let _ = write!(stdout, "\r\n[remuda] {name} ended — press any key\r\n");
        let _ = stdout.flush();
        Left::Exited
    };
    #[cfg(windows)]
    if taken_over {
        // ConPTY stdin may be blocked in a synchronous console read. Once the
        // takeover outcome is known, returning lets the CLI exit and Windows
        // tear down that worker instead of waiting forever in `join`.
        drop(keys);
    } else {
        let _ = keys.join();
    }
    #[cfg(unix)]
    let _ = keys.join();
    Ok(left)
}

#[cfg(windows)]
fn wait_for_windows_keypress() -> std::io::Result<()> {
    use windows_sys::Win32::System::Console::{
        GetStdHandle, ReadConsoleInputW, INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE,
    };

    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut records = [INPUT_RECORD::default(); 16];
    loop {
        let mut count = 0;
        if unsafe {
            ReadConsoleInputW(
                handle,
                records.as_mut_ptr(),
                records.len() as u32,
                &mut count,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        for record in records.iter().take(count as usize) {
            if u32::from(record.EventType) == KEY_EVENT {
                // Console input queues include key-up records as well. Only a
                // key-down should release the post-exit wait.
                let key = unsafe { record.Event.KeyEvent };
                if key.bKeyDown != 0 && key.wVirtualKeyCode != 0 {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(unix)]
fn read_stdin_timeout(
    _stdin: &mut impl Read,
    buf: &mut [u8],
    timeout: std::time::Duration,
) -> std::io::Result<Option<usize>> {
    use std::os::fd::AsRawFd;
    let fd = std::io::stdin().as_raw_fd();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let millis = remaining.as_millis().min(i32::MAX as u128) as i32;
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, millis) };
        if ready > 0 {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            return if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(Some(n as usize))
            };
        }
        if ready == 0 {
            return Ok(None);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

#[cfg(windows)]
fn read_stdin_timeout(
    stdin: &mut impl Read,
    buf: &mut [u8],
    timeout: std::time::Duration,
) -> std::io::Result<Option<usize>> {
    use windows_sys::Win32::{
        Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::{
            Console::{GetStdHandle, STD_INPUT_HANDLE},
            Threading::WaitForSingleObject,
        },
    };
    let handle: HANDLE = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
    match unsafe { WaitForSingleObject(handle, millis) } {
        WAIT_OBJECT_0 => stdin.read(buf).map(Some),
        WAIT_TIMEOUT => Ok(None),
        _ => Err(std::io::Error::last_os_error()),
    }
}

fn route_tokens(
    path: &Path,
    name: &str,
    stream: &mut Stream,
    tokens: Vec<crate::mouse::InputToken>,
    mouse_on: &mut bool,
    scrollback: &std::sync::atomic::AtomicUsize,
    output_lock: &std::sync::Mutex<()>,
) {
    use crate::mouse::{route_mouse_event, InputToken, MouseAction};
    use remuda_core::agent::MouseState;
    use std::sync::atomic::Ordering;

    for token in tokens {
        match token {
            InputToken::Mouse(event) => {
                let state = if *mouse_on {
                    match request(path, &Request::MouseState { name: name.into() }) {
                        Ok(Response::MouseState(state)) => state,
                        _ => MouseState::default(),
                    }
                } else {
                    MouseState::default()
                };
                match route_mouse_event(event, state, *mouse_on, scrollback.load(Ordering::SeqCst))
                {
                    MouseAction::Forward(bytes) => {
                        let _ = stream.write_all(&bytes);
                        let _ = stream.flush();
                    }
                    MouseAction::Scroll(next) => {
                        let _guard = output_lock.lock().unwrap_or_else(|e| e.into_inner());
                        scrollback.store(next, Ordering::SeqCst);
                        paint_history(path, name, next);
                    }
                    MouseAction::Ignore => {}
                }
            }
            InputToken::Bytes(bytes) => {
                if *mouse_on && (bytes == b"\x1b[5~" || bytes == b"\x1b[6~") {
                    let _guard = output_lock.lock().unwrap_or_else(|e| e.into_inner());
                    let old = scrollback.load(Ordering::SeqCst);
                    let next = if bytes == b"\x1b[5~" {
                        (old + 24).min(10_000)
                    } else {
                        old.saturating_sub(24)
                    };
                    if next != 0 || old != 0 {
                        scrollback.store(next, Ordering::SeqCst);
                        paint_history(path, name, next);
                        continue;
                    }
                }
                let mut start = 0;
                for (at, &byte) in bytes.iter().enumerate() {
                    if byte == 0x1d {
                        if start < at {
                            exit_history_if_needed(
                                path,
                                name,
                                stream,
                                &bytes[start..at],
                                scrollback,
                                output_lock,
                            );
                        }
                        *mouse_on = !*mouse_on;
                        let mut stdout = std::io::stdout();
                        let _guard = output_lock.lock().unwrap_or_else(|e| e.into_inner());
                        let report = if *mouse_on {
                            b"\x1b[?1000h\x1b[?1006h"
                        } else {
                            b"\x1b[?1000l\x1b[?1006l"
                        };
                        let _ = stdout.write_all(report);
                        let _ = stdout.flush();
                        start = at + 1;
                    }
                }
                if start < bytes.len() {
                    exit_history_if_needed(
                        path,
                        name,
                        stream,
                        &bytes[start..],
                        scrollback,
                        output_lock,
                    );
                }
            }
            InputToken::Paste(bytes) => {
                let _ = stream.write_all(&bytes);
                let _ = stream.flush();
            }
        }
    }
}

fn exit_history_if_needed(
    path: &Path,
    name: &str,
    stream: &mut Stream,
    bytes: &[u8],
    scrollback: &std::sync::atomic::AtomicUsize,
    output_lock: &std::sync::Mutex<()>,
) {
    use std::sync::atomic::Ordering;
    let old = scrollback.load(Ordering::SeqCst);
    if old != 0 {
        let _guard = output_lock.lock().unwrap_or_else(|e| e.into_inner());
        if bytes == b"q" || bytes == b"\x1b" || bytes == b"\x1bq" {
            scrollback.store(0, Ordering::SeqCst);
            paint_history(path, name, 0);
            return;
        }
        scrollback.store(0, Ordering::SeqCst);
        paint_history(path, name, 0);
    }
    let _ = stream.write_all(bytes);
    let _ = stream.flush();
}

/// Paint one captured frame while the caller holds the output lock.
fn paint_history(path: &Path, name: &str, offset: usize) {
    use remuda_core::agent::Color;
    let Ok(Response::StyledScreen { rows, cursor, .. }) = request(
        path,
        &Request::CaptureStyled {
            name: name.into(),
            scrollback: offset,
        },
    ) else {
        return;
    };
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"\x1b[H\x1b[2J");
    for (index, row) in rows.iter().enumerate() {
        for run in row {
            let mut codes = vec![
                if run.bold { "1" } else { "22" }.to_string(),
                if run.dim { "2" } else { "22" }.to_string(),
                if run.italic { "3" } else { "23" }.to_string(),
                if run.underline { "4" } else { "24" }.to_string(),
                if run.inverse { "7" } else { "27" }.to_string(),
            ];
            let color_codes = |color: Color, foreground: bool| match color {
                Color::Default => vec![if foreground { "39" } else { "49" }.to_string()],
                Color::Idx(index) => vec![
                    if foreground { "38" } else { "48" }.to_string(),
                    "5".into(),
                    index.to_string(),
                ],
                Color::Rgb(r, g, b) => vec![
                    if foreground { "38" } else { "48" }.to_string(),
                    "2".into(),
                    r.to_string(),
                    g.to_string(),
                    b.to_string(),
                ],
            };
            codes.extend(color_codes(run.fg, true));
            codes.extend(color_codes(run.bg, false));
            let _ = write!(stdout, "\x1b[{}m", codes.join(";"));
            let _ = stdout.write_all(run.text.as_bytes());
            let _ = stdout.write_all(b"\x1b[0m");
        }
        if index + 1 < rows.len() {
            let _ = stdout.write_all(b"\r\n");
        }
    }
    if offset != 0 {
        let _ = write!(
            stdout,
            "\x1b[{};1H\x1b[2K[scrollback: {offset} rows — PgUp/PgDn, q/Esc returns]",
            rows.len().max(1)
        );
    }
    if offset == 0 {
        let _ = if cursor.visible {
            write!(
                stdout,
                "\x1b[{};{}H\x1b[?25h",
                cursor.row + 1,
                cursor.col + 1
            )
        } else {
            stdout.write_all(b"\x1b[?25l")
        };
    } else {
        let _ = stdout.write_all(b"\x1b[?25l");
    }
    let _ = stdout.flush();
}

/// A session held for a human typing into a pane rather than into the whole
/// terminal. The same exclusive `Attach` a ride takes, so orchestrated input is
/// refused while a person drives — PRINCIPLES §6, invariant 3.
pub struct Hold {
    stream: Stream,
    drain: Option<std::thread::JoinHandle<()>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Take a session for the TUI's focused pane. The output is *not* handed back:
/// a pane repaints from `Capture`, which crops to its own width and cannot be
/// fed raw pty bytes aimed at a whole terminal.
pub fn hold(path: &Path, name: &str) -> std::io::Result<Hold> {
    hold_inner(path, name, std::time::Duration::ZERO)
}

/// [TEST-ONLY] Like `hold`, but the drain thread sleeps `drain_delay` before
/// its first read, forcing the Windows same-thread-drop race regardless of
/// caller timing. See steps/029.
pub fn hold_with_drain_delay(
    path: &Path,
    name: &str,
    drain_delay: std::time::Duration,
) -> std::io::Result<Hold> {
    hold_inner(path, name, drain_delay)
}

fn hold_inner(path: &Path, name: &str, drain_delay: std::time::Duration) -> std::io::Result<Hold> {
    let stream = ipc::connect(path)?;
    send(
        &stream,
        &Request::Attach {
            name: name.to_string(),
        },
    )?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    match interpret(&line) {
        Response::Ok => {}
        Response::Error(reason) => return Err(std::io::Error::other(reason)),
        _ => return Err(std::io::Error::other("daemon did not acknowledge attach")),
    }
    // Drained rather than ignored: the daemon repaints and then streams, and an
    // unread socket fills and parks its output pump.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let drain_stop = stop.clone();
    let drain = std::thread::spawn(move || {
        if !drain_delay.is_zero() {
            std::thread::sleep(drain_delay);
        }
        let mut buf = [0u8; 8192];
        while !drain_stop.load(std::sync::atomic::Ordering::SeqCst) {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });
    Ok(Hold {
        stream,
        drain: Some(drain),
        stop,
    })
}

impl Hold {
    /// Type exactly these bytes: one `write_all`, nothing appended — the terms
    /// `Attached::write_raw` sets one layer down.
    pub fn keys(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut stream = &self.stream;
        stream.write_all(bytes)?;
        stream.flush()
    }
}

impl Drop for Hold {
    /// Hanging up is what releases the guard, so it has to survive a panic or an
    /// early return — a guard outliving its viewer is the deadlock step 012 met.
    fn drop(&mut self) {
        if let Some(drain) = self.drain.take() {
            ipc::stop_reader(&self.stream, &self.stop, || drain.is_finished());
            let _ = drain.join();
        }
    }
}

/// Raw mode plus the alternate screen, restored on the way out. Restoring must
/// stay in `Drop`: the exits that matter — an error, a panic, a `?` — are
/// exactly the ones that skip the end of `attach`.
pub struct RawMode;

impl RawMode {
    /// The alternate screen is what makes leaving *visible*: your scrollback
    /// and prompt come back, so a second `exit` cannot be aimed at the wrong
    /// shell by a screen that never changed.
    pub fn enable() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        if let Err(e) =
            crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)
        {
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(e);
        }
        Ok(Self)
    }
}

impl Drop for RawMode {
    /// Leave the alternate screen *before* termios goes back, so the last thing
    /// the terminal does in raw mode is the buffer switch.
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        let _ = reset_input_modes(&mut stdout);
        let _ = crossterm::execute!(
            stdout,
            crossterm::cursor::Show,
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        // Keep the protocol reset and alternate-screen exit ordered on the
        // terminal before process shutdown, including on Windows where stdout
        // may still have buffered writes at this point.
        let _ = stdout.flush();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::trace_input_read;
    use super::{interpret, reset_input_modes, write_input_trace, RESET_INPUT_MODES};
    use remuda_core::protocol::Response;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn detach_resets_mouse_and_bracketed_paste_modes() {
        let mut output = Vec::new();
        reset_input_modes(&mut output).unwrap();
        assert_eq!(output, RESET_INPUT_MODES);
    }

    #[test]
    fn input_trace_records_a_timestamp_and_each_byte_as_hex() {
        let mut output = Vec::new();
        // Windows SystemTime has 100 ns precision, so keep the fixture on that
        // clock's representable grid as well.
        let at = UNIX_EPOCH + Duration::new(7, 420_000_000);
        write_input_trace(&mut output, at, b"\x1b\xff").unwrap();
        assert_eq!(output, b"7.420000000 1b ff\n");
    }

    #[cfg(unix)]
    #[test]
    fn input_trace_is_private_when_created_and_when_reusing_a_loose_file() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "remuda-input-trace-mode-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_file(&path);

        trace_input_read(Some(&path), b"secret");
        let private_mode = || std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(private_mode() & 0o077, 0, "new trace file must be private");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        trace_input_read(Some(&path), b"secret again");
        assert_eq!(
            private_mode() & 0o077,
            0,
            "existing trace file must be made private"
        );
        let _ = std::fs::remove_file(path);
    }

    /// The line the daemon at 618cda4^ actually sent, byte for byte.
    const STALE: &str =
        r#"{"Error":"bad request: invalid type: null, expected a string at line 1 column 19"}"#;

    #[test]
    fn a_daemon_that_cannot_read_us_is_named_as_a_stale_build() {
        let Response::Error(said) = interpret(STALE) else {
            panic!("not an error")
        };
        assert!(said.starts_with("the daemon is not this build"), "{said}");
        // The serde detail is the real diagnostic; it must survive, just not lead.
        assert!(said.contains("invalid type: null"), "{said}");
    }

    #[test]
    fn an_unknown_variant_is_the_same_diagnosis() {
        // What an old daemon says to `Version` — the shape step 015 must not
        // turn into something more confusing than what it replaced.
        let line = r#"{"Error":"bad request: unknown variant `Version`, expected one of `List`, `New` at line 1 column 11"}"#;
        let Response::Error(said) = interpret(line) else {
            panic!("not an error")
        };
        assert!(said.starts_with("the daemon is not this build"), "{said}");
    }

    /// The TUI footer is one row wide and crops the tail, so a message that
    /// leads with the diagnosis loses the cure off the right edge. Measured at
    /// 80 columns with the 8-character "remuda: " prefix a caller adds.
    #[test]
    fn the_cure_survives_an_eighty_column_crop() {
        let Response::Error(said) = interpret(STALE) else {
            panic!("not an error")
        };
        let footer: String = format!("remuda: {said}").chars().take(80).collect();
        assert!(
            footer.contains("remuda stop"),
            "cropped away the cure:\n{footer}"
        );
    }

    /// The negative control: a refusal about CONTENT is not skew and must reach
    /// the user in the daemon's own words. Without this the fix above would
    /// swallow every error into one sentence and be indistinguishable from it.
    #[test]
    fn an_ordinary_refusal_is_passed_through_untouched() {
        let line = r#"{"Error":"no such session: build"}"#;
        assert_eq!(
            interpret(line),
            Response::Error("no such session: build".into())
        );
    }

    #[test]
    fn a_hangup_is_not_reported_as_a_parse_failure() {
        let Response::Error(said) = interpret("") else {
            panic!("not an error")
        };
        assert!(said.contains("hung up"), "{said}");
    }

    #[test]
    fn a_normal_reply_still_arrives_as_itself() {
        assert_eq!(interpret(r#""Ok""#), Response::Ok);
    }
}
