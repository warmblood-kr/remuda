use gpui::{actions, AnyWindowHandle, App, KeyBinding, Menu, MenuItem, Window, WindowBounds};
use objc2::rc::Retained;
#[cfg(not(test))]
use objc2_app_kit::NSView;
use objc2_app_kit::{NSScreen, NSWindow};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};
#[cfg(not(test))]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use remuda_native::fs_lock::{self, Outcome};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::{
        fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
        net::UnixDatagram,
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

actions!(remuda_desktop, [Quit, CloseWindow]);

pub(crate) struct PrimaryWindow {
    socket: UnixDatagram,
    path: PathBuf,
    _lock: File,
}

impl PrimaryWindow {
    /// A second launch raises the first GUI, including launches from a tray
    /// or an executable belonging to a different installed nightly.
    pub(crate) fn acquire() -> io::Result<Option<Self>> {
        Self::acquire_in(&remuda_native::daemon::runtime_dir())
    }

    fn acquire_in(runtime: &Path) -> io::Result<Option<Self>> {
        fs::create_dir_all(runtime)?;
        let path = runtime.join("gui.sock");
        let lock = runtime.join("remuda-gui.lock");
        let client = UnixDatagram::unbound()?;
        client.set_nonblocking(true)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match fs_lock::acquire(&lock, &fs_lock::info_line("gui"))? {
                Outcome::Acquired(lock) => {
                    match fs::symlink_metadata(&path) {
                        Ok(meta) if meta.file_type().is_socket() => fs::remove_file(&path)?,
                        Ok(_) => {
                            return Err(io::Error::other("GUI activation path is not a socket"))
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                    let socket = UnixDatagram::bind(&path)?;
                    let primary = Self {
                        socket,
                        path,
                        _lock: lock,
                    };
                    fs::set_permissions(&primary.path, fs::Permissions::from_mode(0o600))?;
                    primary.socket.set_nonblocking(true)?;
                    return Ok(Some(primary));
                }
                Outcome::Held(_) => match client.send_to(b"open", &path) {
                    Ok(_) => return Ok(None),
                    Err(error) if Instant::now() >= deadline => return Err(error),
                    Err(_) => std::thread::sleep(Duration::from_millis(25)),
                },
            }
        }
    }

    pub(crate) fn take_open_request(&self) -> bool {
        let mut activate = false;
        let mut message = [0; 16];
        for _ in 0..32 {
            match self.socket.recv(&mut message) {
                Ok(4) if &message[..4] == b"open" => activate = true,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        activate
    }
}

impl Drop for PrimaryWindow {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Clone, Copy)]
pub(crate) enum WindowKind {
    Remuda,
    Browser,
}

#[derive(Serialize, Deserialize)]
struct Geometry {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Geometry {
    fn frame(&self) -> Option<NSRect> {
        ([self.x, self.y, self.width, self.height]
            .iter()
            .all(|n| n.is_finite())
            && self.width >= 160.
            && self.height >= 120.)
            .then(|| {
                NSRect::new(
                    NSPoint::new(self.x, self.y),
                    NSSize::new(self.width, self.height),
                )
            })
    }
}

#[cfg(not(test))]
fn native_window(window: &Window) -> Option<Retained<NSWindow>> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    // GPUI owns this NSView for the lifetime of the borrowed window handle.
    let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    view.window()
}

// GPUI's TestWindow cannot supply a native handle.
#[cfg(test)]
fn native_window(_: &Window) -> Option<Retained<NSWindow>> {
    None
}

fn geometry_path(kind: WindowKind) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(base.join("remuda").join(match kind {
        WindowKind::Remuda => "gui-window.json",
        WindowKind::Browser => "browser-window.json",
    }))
}

fn restore(kind: WindowKind, window: &Window, cx: &App) {
    let Some(frame) = geometry_path(kind)
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Geometry>(&bytes).ok())
        .and_then(|geometry| geometry.frame())
    else {
        return;
    };
    let (Some(mtm), Some(native)) = (MainThreadMarker::new(), native_window(window)) else {
        return;
    };
    let screens = NSScreen::screens(mtm);
    let has_visible_title = (0..screens.count()).any(|index| {
        let screen = screens.objectAtIndex(index);
        let visible = screen.visibleFrame();
        let top = frame.origin.y + frame.size.height;
        let horizontal = (frame.origin.x + frame.size.width)
            .min(visible.origin.x + visible.size.width)
            - frame.origin.x.max(visible.origin.x);
        horizontal >= 120.
            && top >= visible.origin.y + 32.
            && top <= visible.origin.y + visible.size.height + 16.
    });
    if has_visible_title {
        // AppKit can call GPUI's bounds callbacks synchronously. Move the
        // frame after this application update has released its borrow.
        cx.foreground_executor()
            .spawn(async move {
                native.setFrame_display(frame, true);
            })
            .detach();
    }
}

fn save(kind: WindowKind, window: &Window) {
    let result = (|| -> io::Result<()> {
        let Some(path) = geometry_path(kind) else {
            return Ok(());
        };
        if matches!(window.window_bounds(), WindowBounds::Fullscreen(_)) {
            return Ok(());
        }
        let Some(native) = native_window(window) else {
            return Ok(());
        };
        let frame = native.frame();
        let geometry = Geometry {
            x: frame.origin.x,
            y: frame.origin.y,
            width: frame.size.width,
            height: frame.size.height,
        };
        let Some(directory) = path.parent() else {
            return Ok(());
        };
        fs::create_dir_all(directory)?;
        let temp = path.with_extension(format!("{}.tmp", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        let result = (|| {
            file.write_all(&serde_json::to_vec(&geometry)?)?;
            fs::rename(&temp, &path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    })();
    if let Err(error) = result {
        eprintln!("remuda gui: could not save window geometry: {error}");
    }
}

pub(crate) fn install_window(window: &Window, cx: &App, kind: WindowKind) {
    restore(kind, window, cx);
    window.on_window_should_close(cx, move |window, _| {
        save(kind, window);
        true
    });
}

pub(crate) fn save_window(handle: AnyWindowHandle, cx: &mut App, kind: WindowKind) {
    let _ = handle.update(cx, |_, window, _| save(kind, window));
}

pub(crate) fn install_menu(cx: &mut App, kind: WindowKind) {
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_app_quit(move |cx| {
        if let Some(window) = cx.windows().first().copied() {
            save_window(window, cx, kind);
        }
        std::future::ready(())
    })
    .detach();
    cx.on_action(move |_: &CloseWindow, cx| {
        if let Some(window) = cx.active_window() {
            let _ = window.update(cx, |_, window, _| {
                save(kind, window);
                window.remove_window();
            });
        }
    });
    cx.set_menus(vec![Menu {
        name: "Remuda".into(),
        items: vec![
            MenuItem::action("Close Window", CloseWindow),
            MenuItem::separator(),
            MenuItem::action(
                match kind {
                    WindowKind::Remuda => "Quit Remuda GUI",
                    WindowKind::Browser => "Quit Remuda Browser",
                },
                Quit,
            ),
        ],
    }]);
}

pub(crate) fn activate(window: AnyWindowHandle, cx: &mut App) {
    cx.activate(true);
    let _ = window.update(cx, |_, window, cx| {
        if let Some(native) = native_window(window).filter(|window| window.isMiniaturized()) {
            cx.foreground_executor()
                .spawn(async move {
                    native.deminiaturize(None);
                })
                .detach();
        }
        window.activate_window();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an isolated Unix datagram socket"]
    fn second_launch_signals_primary_and_closed_primary_can_be_replaced() {
        let path = PathBuf::from(format!("/tmp/rgui-test-{}", std::process::id()));
        fs::create_dir(&path).unwrap();
        let primary = PrimaryWindow::acquire_in(&path).unwrap().unwrap();
        assert!(!primary.take_open_request());
        assert!(PrimaryWindow::acquire_in(&path).unwrap().is_none());
        assert!(primary.take_open_request());
        assert!(!primary.take_open_request());
        drop(primary);

        let stale = UnixDatagram::bind(path.join("gui.sock")).unwrap();
        drop(stale);
        let new_primary = PrimaryWindow::acquire_in(&path).unwrap().unwrap();
        assert!(!new_primary.take_open_request());
        drop(new_primary);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn geometry_round_trips_positions_on_other_displays() {
        let geometry = Geometry {
            x: -1400.,
            y: 35.,
            width: 1000.,
            height: 650.,
        };
        let bytes = serde_json::to_vec(&geometry).unwrap();
        let frame = serde_json::from_slice::<Geometry>(&bytes)
            .unwrap()
            .frame()
            .unwrap();
        assert_eq!(frame.origin.x, -1400.);
        assert_eq!(frame.size.width, 1000.);
        assert!(Geometry {
            width: 0.,
            ..geometry
        }
        .frame()
        .is_none());
    }
}
