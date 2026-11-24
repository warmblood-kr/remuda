use std::{
    io,
    ptr::{null, null_mut},
};
use windows_sys::{
    w,
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
                NOTIFYICONDATAW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
                DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW,
                PostMessageW, RegisterClassW, SetForegroundWindow, TrackPopupMenu,
                UnregisterClassW, IDI_APPLICATION, MF_STRING, MSG, TPM_RETURNCMD, TPM_RIGHTBUTTON,
                WM_APP, WM_CONTEXTMENU, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
            },
        },
    },
};

const ICON_EVENT: u32 = WM_APP + 1;
const MENU_ACTION: u32 = WM_APP + 2;
const OPEN_REMUDA: usize = 1;
const QUIT_TRAY: usize = 2;
const OPEN_BROWSER: usize = 3;

pub(super) fn run() -> io::Result<()> {
    // SAFETY: Use a process-owned class and hidden window on this thread. The
    // class name is static, and the class is unregistered after its window ends.
    unsafe {
        let instance = GetModuleHandleW(null());
        if instance.is_null() {
            return Err(io::Error::last_os_error());
        }
        let name = w!("RemudaTrayWindow");
        let class = WNDCLASSW {
            hInstance: instance,
            lpszClassName: name,
            lpfnWndProc: Some(window_proc),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(io::Error::last_os_error());
        }
        let window = CreateWindowExW(
            0,
            name,
            w!("Remuda"),
            0,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        let result = if window.is_null() {
            Err(io::Error::last_os_error())
        } else {
            let result = run_icon(window);
            DestroyWindow(window);
            result
        };
        UnregisterClassW(name, instance);
        result
    }
}

fn run_icon(window: HWND) -> io::Result<()> {
    // SAFETY: The caller's thread owns the window and keeps it alive throughout
    // the message loop. The stock icon belongs to the system.
    unsafe {
        let icon = LoadIconW(null_mut(), IDI_APPLICATION);
        if icon.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut tooltip = [0; 128];
        for (slot, value) in tooltip.iter_mut().zip("Remuda".encode_utf16()) {
            *slot = value;
        }
        let data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: window,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: ICON_EVENT,
            hIcon: icon,
            szTip: tooltip,
            ..Default::default()
        };
        if Shell_NotifyIconW(NIM_ADD, &data) == 0 {
            return Err(io::Error::other("cannot add the Windows notification icon"));
        }
        let result = run_message_loop(window);
        Shell_NotifyIconW(NIM_DELETE, &data);
        result
    }
}

fn run_message_loop(window: HWND) -> io::Result<()> {
    // SAFETY: GetMessageW fills a writable MSG. When it succeeds, dispatch
    // messages on the thread that created the window.
    unsafe {
        let mut message = MSG::default();
        loop {
            match GetMessageW(&mut message, null_mut(), 0, 0) {
                -1 => return Err(io::Error::last_os_error()),
                0 => return Ok(()),
                _ if message.hwnd == window && message.message == MENU_ACTION => {
                    match message.wParam {
                        OPEN_REMUDA => super::open_remuda(),
                        OPEN_BROWSER => super::open_browser(),
                        QUIT_TRAY => return Ok(()),
                        _ => {}
                    }
                }
                _ => {
                    DispatchMessageW(&message);
                }
            }
        }
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == ICON_EVENT
        && matches!(lparam as u32, WM_LBUTTONUP | WM_RBUTTONUP | WM_CONTEXTMENU)
    {
        // SAFETY: Win32 invokes this procedure on the window's owner thread.
        unsafe { show_menu(window) };
        return 0;
    }
    // SAFETY: Forward Win32's message arguments to the default procedure.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

unsafe fn show_menu(window: HWND) {
    // SAFETY: Only the owning thread calls this with a live window. The popup
    // is private to this function and is destroyed after menu tracking ends.
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let mut cursor = POINT::default();
        if AppendMenuW(menu, MF_STRING, OPEN_REMUDA, w!("Open Remuda")) != 0
            && AppendMenuW(menu, MF_STRING, OPEN_BROWSER, w!("Open Browser…")) != 0
            && AppendMenuW(menu, MF_STRING, QUIT_TRAY, w!("Quit Tray")) != 0
            && GetCursorPos(&mut cursor) != 0
        {
            SetForegroundWindow(window);
            let action = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                cursor.x,
                cursor.y,
                0,
                window,
                null(),
            );
            // Wake the window after menu tracking so later popups can dismiss normally.
            PostMessageW(window, WM_NULL, 0, 0);
            if action > 0 {
                PostMessageW(window, MENU_ACTION, action as usize, 0);
            }
        }
        DestroyMenu(menu);
    }
}
