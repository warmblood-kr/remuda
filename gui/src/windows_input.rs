//! Let Windows handle the IME mode key before GPUI interprets key messages.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::{
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{DefWindowProcW, WM_KEYDOWN, WM_KEYUP, WM_NCDESTROY},
    },
};

const IME_SUBCLASS: usize = 1;
const VK_HANGUL: WPARAM = 0x15; // Also VK_KANA on a Japanese keyboard.

pub(crate) fn install(window: &gpui::Window) {
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    // The GPUI window and its subclass both run on the UI thread.
    if unsafe { SetWindowSubclass(handle.hwnd.get() as HWND, Some(ime_key), IME_SUBCLASS, 0) } == 0
    {
        eprintln!("Remuda: could not install Windows IME key handler");
    }
}

unsafe extern "system" fn ime_key(
    hwnd: HWND,
    message: u32,
    key: WPARAM,
    data: LPARAM,
    id: usize,
    _: usize,
) -> LRESULT {
    if matches!(message, WM_KEYDOWN | WM_KEYUP) && key == VK_HANGUL {
        return unsafe { DefWindowProcW(hwnd, message, key, data) };
    }
    if message == WM_NCDESTROY {
        unsafe { RemoveWindowSubclass(hwnd, Some(ime_key), id) };
    }
    unsafe { DefSubclassProc(hwnd, message, key, data) }
}
