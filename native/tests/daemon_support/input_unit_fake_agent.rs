use crossterm::terminal;
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

fn main() {
    terminal::enable_raw_mode().expect("enable raw terminal mode");
    match std::env::args().nth(1).as_deref() {
        Some("empty") => empty_agent(),
        Some("silent") => silent_agent(),
        Some("paste") => paste_agent(),
        mode => panic!("unknown fake agent mode: {mode:?}"),
    }
}

fn reader() -> Receiver<(Vec<u8>, Instant)> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut stdin = io::stdin();
        loop {
            let mut bytes = vec![0; 4096];
            match stdin.read(&mut bytes) {
                Ok(0) | Err(_) => return,
                Ok(length) => {
                    bytes.truncate(length);
                    if sender.send((bytes, Instant::now())).is_err() {
                        return;
                    }
                }
            }
        }
    });
    receiver
}

fn write(bytes: &[u8]) {
    let mut stdout = io::stdout().lock();
    stdout.write_all(bytes).expect("write terminal output");
    stdout.flush().expect("flush terminal output");
}

fn empty_agent() {
    write(b"\x1b[?2004hREADY");
    let receiver = reader();
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut bytes = Vec::new();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok((chunk, _)) => bytes.extend_from_slice(&chunk),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if bytes == b"\r" {
        write(b"\r\nBARE_RETURN");
    } else {
        write(format!("\r\nBYTES:{}", hex(&bytes)).as_bytes());
    }
    thread::sleep(Duration::from_secs(2));
}

fn silent_agent() {
    write(b"\x1b[?2004hREADY\r\n> \r\nSTATUS");
    let receiver = reader();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buffer = Vec::new();
    let mut returns = 0;
    let mut reported = false;
    while Instant::now() < deadline {
        match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok((data, _)) => {
                let mut index = 0;
                while index < data.len() {
                    let byte = data[index];
                    if byte == b'\r' {
                        returns += 1;
                        buffer.clear();
                    } else {
                        buffer.push(byte);
                        redraw_silent(&buffer);
                    }
                    index += 1;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if !reported && Instant::now() > deadline - Duration::from_millis(300) {
            write(format!("\r\nRETURNS:{returns}").as_bytes());
            reported = true;
        }
    }
}

fn redraw_silent(buffer: &[u8]) {
    let mut output = b"\x1b[H\x1b[2JREADY\r\n> ".to_vec();
    output.extend_from_slice(buffer);
    output.extend_from_slice(b"\r\nSTATUS");
    write(&output);
}

fn paste_agent() {
    write(b"\x1b[?2004hREADY\r\n> \r\nSTATUS ONE\r\nSTATUS TWO");
    let receiver = reader();
    let mut buffer = Vec::new();
    let mut last = None;
    let mut paste = false;
    let mut submitted = 0;
    let mut returns = 0;
    let mut transcript = Vec::new();
    loop {
        let Ok((data, arrived)) = receiver.recv_timeout(Duration::from_secs(5)) else {
            continue;
        };
        if !transcript.is_empty() && data.starts_with(PASTE_START) {
            // A real TUI can need several frames to repaint a pasted composer.
            thread::sleep(Duration::from_millis(300));
        }
        let mut index = 0;
        while index < data.len() {
            if data[index..].starts_with(PASTE_START) {
                paste = true;
                index += PASTE_START.len();
                continue;
            }
            if data[index..].starts_with(PASTE_END) {
                paste = false;
                index += PASTE_END.len();
                continue;
            }
            let byte = data[index];
            if byte == b'\r' {
                returns += 1;
                if paste
                    || last.is_some_and(|previous| {
                        arrived.duration_since(previous) < Duration::from_millis(500)
                    })
                {
                    buffer.push(b'\n');
                    redraw_paste(&buffer, &transcript);
                } else {
                    submitted += 1;
                    let mut display = Vec::new();
                    for byte in &buffer {
                        if *byte == b'\n' {
                            display.extend_from_slice(b"\\n");
                        } else {
                            display.push(*byte);
                        }
                    }
                    let mut record = b"SUBMITTED:".to_vec();
                    record.extend_from_slice(&display);
                    record.extend_from_slice(
                        format!("\r\nCOUNT:{submitted}\r\nRETURNS:{returns}\r\n").as_bytes(),
                    );
                    transcript.extend_from_slice(&record);
                    write(b"\r\n");
                    write(&record);
                    buffer.clear();
                }
            } else {
                buffer.push(byte);
                redraw_paste(&buffer, &transcript);
            }
            last = Some(arrived);
            index += 1;
        }
    }
}

fn redraw_paste(buffer: &[u8], transcript: &[u8]) {
    let mut display = Vec::new();
    for byte in buffer {
        if *byte == b'\n' {
            display.extend_from_slice(b"\r\n  ");
        } else {
            display.push(*byte);
        }
    }
    let mut output = b"\x1b[H\x1b[2JREADY\r\n".to_vec();
    output.extend_from_slice(transcript);
    output.extend_from_slice(b"\r\n> ");
    output.extend_from_slice(&display);
    output.extend_from_slice(b"\r\nSTATUS ONE\r\nSTATUS TWO");
    write(&output);
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    result
}
