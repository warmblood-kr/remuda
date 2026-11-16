//! An embedded PTY running the existing interactive attach client.
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};

const ROWS: u16 = 24;
const COLS: u16 = 80;

pub struct Terminal {
    child: Box<dyn Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
    input: mpsc::SyncSender<Vec<u8>>,
    parser: Arc<Mutex<vt100::Parser>>,
    ended: Arc<AtomicBool>,
}

impl Terminal {
    pub fn start(executable: &Path, name: &str, instance: &str, abi: &str) -> Result<Self, String> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;
        let mut command = CommandBuilder::new(executable);
        command.args([
            "--internal-preview-worker",
            "local",
            "",
            name,
            instance,
            "--terminal",
            "--attach-v1",
            abi,
        ]);
        command.env("TERM", "xterm-256color");
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| e.to_string())?;
        drop(pair.slave);
        let reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
        };
        let writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
        };
        let parser = Arc::new(Mutex::new(vt100::Parser::new(ROWS, COLS, 0)));
        let ended = Arc::new(AtomicBool::new(false));
        let (input, receiver) = mpsc::sync_channel::<Vec<u8>>(64);
        std::thread::spawn({
            let parser = parser.clone();
            let ended = ended.clone();
            move || {
                let mut reader = reader;
                let mut bytes = [0; 8192];
                while let Ok(n) = reader.read(&mut bytes) {
                    if n == 0 {
                        break;
                    }
                    if let Ok(mut parser) = parser.lock() {
                        parser.process(&bytes[..n]);
                    } else {
                        break;
                    }
                }
                ended.store(true, Ordering::Release);
            }
        });
        std::thread::spawn(move || {
            let mut writer = writer;
            while let Ok(bytes) = receiver.recv() {
                if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            _master: pair.master,
            input,
            parser,
            ended,
        })
    }
    pub fn screen(&self) -> String {
        self.parser
            .lock()
            .map(|p| p.screen().contents())
            .unwrap_or_default()
    }
    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }
    pub fn send(&self, bytes: Vec<u8>) -> Result<(), &'static str> {
        if self.ended() {
            return Err("Terminal ended");
        }
        self.input
            .try_send(bytes)
            .map_err(|_| "Terminal input busy or closed")
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn key_bytes(key: &gpui::Keystroke) -> Option<Vec<u8>> {
    if key.modifiers.platform || key.modifiers.function {
        return None;
    }
    if key.modifiers.control {
        let letter = key.key.as_bytes();
        if letter.len() == 1 {
            let c = letter[0].to_ascii_lowercase();
            if c.is_ascii_lowercase() {
                return Some(vec![c - b'a' + 1]);
            }
            if c == b'\\' {
                return Some(vec![0x1c]);
            }
        }
        return None;
    }
    let bytes = match key.key.as_str() {
        "enter" => b"\r".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "tab" => b"\t".to_vec(),
        "escape" => b"\x1b".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        "home" => b"\x1b[H".to_vec(),
        "end" => b"\x1b[F".to_vec(),
        "delete" => b"\x1b[3~".to_vec(),
        "pageup" => b"\x1b[5~".to_vec(),
        "pagedown" => b"\x1b[6~".to_vec(),
        _ => key.key_char.as_ref()?.as_bytes().to_vec(),
    };
    if key.modifiers.alt {
        let mut result = vec![0x1b];
        result.extend(bytes);
        Some(result)
    } else {
        Some(bytes)
    }
}
