use std::sync::atomic::{AtomicBool, Ordering};

static HIDDEN: AtomicBool = AtomicBool::new(false);

pub fn is_hidden() -> bool {
    HIDDEN.load(Ordering::Relaxed)
}

#[cfg(not(target_os = "windows"))]
pub fn install(_main_hwnd: isize) {}

#[cfg(not(target_os = "windows"))]
pub fn hide_main_window() -> bool {
    false
}

#[cfg(not(target_os = "windows"))]
pub fn allow_close_requested() -> bool {
    true
}

#[cfg(not(target_os = "windows"))]
pub fn shutdown() {}

#[cfg(target_os = "windows")]
pub use windows_impl::{allow_close_requested, hide_main_window, install, shutdown};

#[cfg(target_os = "windows")]
mod windows_impl {
    #![allow(unsafe_op_in_unsafe_fn)]

    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
        DispatchMessageW, GetCursorPos, GetMessageW, IDI_APPLICATION, LoadIconW, MF_STRING, MSG,
        PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SW_HIDE, SW_RESTORE,
        SetForegroundWindow, ShowWindow, TPM_NONOTIFY, TPM_RETURNCMD, TrackPopupMenu,
        TranslateMessage, WM_APP, WM_CLOSE, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP,
        WM_RBUTTONUP, WNDCLASSW,
    };

    use super::HIDDEN;

    const TRAY_ID: u32 = 1;
    const TRAY_MESSAGE: u32 = WM_APP + 1;
    const MENU_SHOW: usize = 1;
    const MENU_QUIT: usize = 2;

    static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
    static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
    static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    static ALLOW_CLOSE: AtomicBool = AtomicBool::new(false);

    pub fn install(main_hwnd: isize) {
        MAIN_HWND.store(main_hwnd, Ordering::SeqCst);
        if INSTALLED.swap(true, Ordering::SeqCst) {
            return;
        }
        std::thread::spawn(|| unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let class_name = wide("WceTrayWindow");
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class_name.as_ptr(),
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                class_name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                INSTALLED.store(false, Ordering::SeqCst);
                return;
            }
            TRAY_HWND.store(hwnd as isize, Ordering::SeqCst);
            TASKBAR_CREATED.store(
                RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
                Ordering::SeqCst,
            );
            add_icon(hwnd);

            let mut message: MSG = std::mem::zeroed();
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            TRAY_HWND.store(0, Ordering::SeqCst);
            INSTALLED.store(false, Ordering::SeqCst);
        });
    }

    pub fn hide_main_window() -> bool {
        let hwnd = MAIN_HWND.load(Ordering::SeqCst);
        if hwnd == 0 {
            return false;
        }
        unsafe {
            ShowWindow(hwnd as HWND, SW_HIDE);
        }
        HIDDEN.store(true, Ordering::SeqCst);
        true
    }

    pub fn allow_close_requested() -> bool {
        ALLOW_CLOSE.load(Ordering::SeqCst)
    }

    pub fn shutdown() {
        let hwnd = TRAY_HWND.load(Ordering::SeqCst);
        if hwnd != 0 {
            unsafe {
                PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0);
            }
        }
    }

    fn show_main() {
        let hwnd = MAIN_HWND.load(Ordering::SeqCst);
        if hwnd == 0 {
            return;
        }
        HIDDEN.store(false, Ordering::SeqCst);
        unsafe {
            ShowWindow(hwnd as HWND, SW_RESTORE);
            SetForegroundWindow(hwnd as HWND);
        }
    }

    fn exit_app() {
        let hwnd = MAIN_HWND.load(Ordering::SeqCst);
        ALLOW_CLOSE.store(true, Ordering::SeqCst);
        if hwnd != 0 {
            unsafe {
                PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0);
            }
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            TRAY_MESSAGE => {
                match (lparam & 0xFFFF) as u32 {
                    WM_LBUTTONUP | WM_LBUTTONDBLCLK => show_main(),
                    WM_RBUTTONUP => show_menu(hwnd),
                    _ => {}
                }
                0
            }
            WM_CLOSE => {
                remove_icon(hwnd);
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ if message != 0 && message == TASKBAR_CREATED.load(Ordering::Relaxed) => {
                add_icon(hwnd);
                0
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }

    unsafe fn show_menu(hwnd: HWND) {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let show = wide("显示主窗口");
        let quit = wide("退出");
        AppendMenuW(menu, MF_STRING, MENU_SHOW, show.as_ptr());
        AppendMenuW(menu, MF_STRING, MENU_QUIT, quit.as_ptr());
        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        SetForegroundWindow(hwnd);
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY,
            cursor.x,
            cursor.y,
            0,
            hwnd,
            std::ptr::null(),
        );
        DestroyMenu(menu);
        match command as usize {
            MENU_SHOW => show_main(),
            MENU_QUIT => exit_app(),
            _ => {}
        }
    }

    unsafe fn add_icon(hwnd: HWND) {
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ID;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = TRAY_MESSAGE;
        data.hIcon = LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
        fill(&mut data.szTip, "微信数据");
        Shell_NotifyIconW(NIM_ADD, &data);
    }

    unsafe fn remove_icon(hwnd: HWND) {
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ID;
        Shell_NotifyIconW(NIM_DELETE, &data);
    }

    fn fill(destination: &mut [u16], text: &str) {
        let mut index = 0;
        for unit in text.encode_utf16() {
            if index + 1 >= destination.len() {
                break;
            }
            destination[index] = unit;
            index += 1;
        }
        destination[index] = 0;
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }
}
