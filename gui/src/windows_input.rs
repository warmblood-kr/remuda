//! Handle the Korean IME mode key before GPUI interprets key messages.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::{
        Input::{
            Ime::{
                ImmGetContext, ImmGetConversionStatus, ImmIsIME, ImmReleaseContext,
                ImmSetConversionStatus, ImmSimulateHotKey, IME_CMODE_NATIVE,
                IME_KHOTKEY_SHAPE_TOGGLE,
            },
            KeyboardAndMouse::{
                GetKeyboardLayout, MapVirtualKeyExW, HKL, MAPVK_VSC_TO_VK_EX, VK_HANGUL, VK_MENU,
                VK_RMENU,
            },
        },
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            DefWindowProcW, WM_KEYDOWN, WM_KEYUP, WM_NCDESTROY, WM_SYSKEYDOWN, WM_SYSKEYUP,
        },
    },
};

const IME_SUBCLASS: usize = 1;
const LANG_KOREAN: usize = 0x12;
const EXTENDED_KEY: LPARAM = 1 << 24;
const REPEATED_KEY: LPARAM = 1 << 30;

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
    if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP) {
        let layout = unsafe { GetKeyboardLayout(0) };
        if korean_mode_key(key, data, layout) {
            if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN)
                && (data & REPEATED_KEY != 0 || toggle_hangul(hwnd))
            {
                return 0;
            }
            return unsafe { DefWindowProcW(hwnd, message, key, data) };
        }
    }
    if message == WM_NCDESTROY {
        unsafe { RemoveWindowSubclass(hwnd, Some(ime_key), id) };
    }
    unsafe { DefSubclassProc(hwnd, message, key, data) }
}

fn korean_mode_key(key: WPARAM, data: LPARAM, layout: HKL) -> bool {
    // VK_HANGUL shares its value with VK_KANA. Respect the active layout.
    if layout as usize & 0x3ff != LANG_KOREAN || unsafe { ImmIsIME(layout) } == 0 {
        return false;
    }
    if key == VK_HANGUL as usize {
        return true;
    }
    if !matches!(key as u16, VK_MENU | VK_RMENU) || data & EXTENDED_KEY == 0 {
        return false;
    }

    // Only treat Right Alt as the mode key if the Korean layout maps it so.
    let scan_code = 0xe000 | ((data as u32 >> 16) & 0xff);
    unsafe { MapVirtualKeyExW(scan_code, MAPVK_VSC_TO_VK_EX, layout) == VK_HANGUL as u32 }
}

fn toggle_hangul(hwnd: HWND) -> bool {
    // Use the IME's own hotkey first to let it settle any active composition.
    unsafe {
        if ImmSimulateHotKey(hwnd, IME_KHOTKEY_SHAPE_TOGGLE) != 0 {
            return true;
        }
        let context = ImmGetContext(hwnd);
        if context.is_null() {
            return false;
        }
        let mut conversion = 0;
        let mut sentence = 0;
        let switched = ImmGetConversionStatus(context, &mut conversion, &mut sentence) != 0
            && ImmSetConversionStatus(context, conversion ^ IME_CMODE_NATIVE, sentence) != 0;
        ImmReleaseContext(hwnd, context);
        switched
    }
}
