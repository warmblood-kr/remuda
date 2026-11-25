//! One embedded Remuda icon for the application, windows and notification area.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{ptr::null, sync::OnceLock};
use windows_sys::Win32::{
    Foundation::{HINSTANCE, HWND},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
        GetSystemMetrics, LoadImageW, SendMessageW, HICON, ICON_BIG, ICON_SMALL, IMAGE_ICON,
        SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, WM_SETICON,
    },
};

const RESOURCE_ID: usize = 101;

pub(crate) fn small() -> HICON {
    static SMALL: OnceLock<usize> = OnceLock::new();
    *SMALL.get_or_init(|| load(SM_CXSMICON, SM_CYSMICON) as usize) as HICON
}

fn load(width_metric: i32, height_metric: i32) -> HICON {
    // These handles are cached for the lifetime of the process.
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(null());
        if instance.is_null() {
            return std::ptr::null_mut();
        }
        LoadImageW(
            instance,
            RESOURCE_ID as *const u16,
            IMAGE_ICON,
            GetSystemMetrics(width_metric),
            GetSystemMetrics(height_metric),
            0,
        ) as HICON
    }
}

pub(crate) fn install(window: &gpui::Window) {
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd = handle.hwnd.get() as HWND;
    static BIG: OnceLock<usize> = OnceLock::new();
    let big = *BIG.get_or_init(|| load(SM_CXICON, SM_CYICON) as usize) as HICON;
    for (kind, icon) in [(ICON_SMALL, small()), (ICON_BIG, big)] {
        if !icon.is_null() {
            // Windows retains this process-lifetime handle for the window.
            unsafe { SendMessageW(hwnd, WM_SETICON, kind as usize, icon as isize) };
        }
    }
}
