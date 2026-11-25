#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy)]
pub enum TrayAction {
    ShowWindow,
    CheckForUpdates,
    RestartToUpdate,
    Quit,
}

#[cfg(windows)]
mod platform {
    use super::TrayAction;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::UI::Shell::{
        ExtractIconW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO,
        NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
        DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, PostMessageW,
        PostQuitMessage, RegisterClassExW, SetForegroundWindow, TrackPopupMenu, TranslateMessage,
        IDI_APPLICATION, MF_SEPARATOR, MF_STRING, MSG, TPM_BOTTOMALIGN, TPM_LEFTALIGN, WM_CLOSE,
        WM_COMMAND, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP, WM_USER,
        WNDCLASSEXW,
    };

    const TRAY_ICON_ID: u32 = 1;
    const WM_TRAY: u32 = WM_USER + 42;
    const WM_UPDATE_LABEL: u32 = WM_USER + 43;
    const WM_SHOW_BALLOON: u32 = WM_USER + 44;

    const IDM_OPEN: usize = 1001;
    const IDM_UPDATE: usize = 1002;
    const IDM_QUIT: usize = 1003;

    type TrayCallback = Arc<dyn Fn(TrayAction) + Send + Sync>;

    struct TrayShared {
        callback: TrayCallback,
        update_label: Mutex<String>,
        update_ready: AtomicBool,
    }

    static SHARED: OnceLock<Arc<TrayShared>> = OnceLock::new();
    static BALLOON_PENDING: OnceLock<Mutex<Option<(String, String)>>> = OnceLock::new();

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn copy_wide_fixed<const N: usize>(dst: &mut [u16; N], src: &str) {
        let mut idx = 0;
        for ch in src.encode_utf16() {
            if idx + 1 >= N {
                break;
            }
            dst[idx] = ch;
            idx += 1;
        }
        if N > 0 {
            dst[idx] = 0;
        }
    }

    fn load_tray_icon() -> isize {
        if let Ok(exe) = std::env::current_exe() {
            let exe_w: Vec<u16> = exe
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let extracted = unsafe { ExtractIconW(null_mut(), exe_w.as_ptr(), 0) };
            if !extracted.is_null() && extracted as isize != 1 {
                return extracted as isize;
            }
        }
        unsafe { LoadIconW(null_mut(), IDI_APPLICATION) as isize }
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_TRAY => {
                let mouse_msg = (lparam & 0xFFFF) as u32;
                if mouse_msg == WM_LBUTTONUP || mouse_msg == WM_LBUTTONDBLCLK {
                    if let Some(shared) = SHARED.get() {
                        (shared.callback)(TrayAction::ShowWindow);
                    }
                    return 0;
                }
                if mouse_msg == WM_RBUTTONUP {
                    show_context_menu(hwnd);
                    return 0;
                }
            }
            WM_COMMAND => {
                let cmd = wparam & 0xFFFF;
                if let Some(shared) = SHARED.get() {
                    match cmd {
                        IDM_OPEN => (shared.callback)(TrayAction::ShowWindow),
                        IDM_UPDATE => {
                            if shared.update_ready.load(Ordering::SeqCst) {
                                (shared.callback)(TrayAction::RestartToUpdate);
                            } else {
                                (shared.callback)(TrayAction::CheckForUpdates);
                            }
                        }
                        IDM_QUIT => (shared.callback)(TrayAction::Quit),
                        _ => {}
                    }
                }
                return 0;
            }
            WM_SHOW_BALLOON => {
                if let Some(m) = BALLOON_PENDING.get()
                    && let Ok(mut guard) = m.lock()
                    && let Some((title, body)) = guard.take()
                {
                    let mut nid = NOTIFYICONDATAW {
                        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                        hWnd: hwnd,
                        uID: TRAY_ICON_ID,
                        uFlags: NIF_INFO,
                        dwInfoFlags: NIIF_INFO,
                        ..Default::default()
                    };
                    copy_wide_fixed(&mut nid.szInfoTitle, &title);
                    copy_wide_fixed(&mut nid.szInfo, &body);
                    unsafe {
                        Shell_NotifyIconW(NIM_MODIFY, &nid);
                    }
                }
                return 0;
            }
            WM_UPDATE_LABEL => {
                if let Some(shared) = SHARED.get()
                    && let Ok(tip) = shared.update_label.lock()
                {
                    let mut nid = NOTIFYICONDATAW {
                        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                        hWnd: hwnd,
                        uID: TRAY_ICON_ID,
                        uFlags: NIF_TIP,
                        ..Default::default()
                    };
                    let tip_str = if shared.update_ready.load(Ordering::SeqCst) {
                        format!("Nod — {}", *tip)
                    } else {
                        "Nod".to_string()
                    };
                    copy_wide_fixed(&mut nid.szTip, &tip_str);
                    unsafe {
                        Shell_NotifyIconW(NIM_MODIFY, &nid);
                    }
                }
                return 0;
            }
            WM_CLOSE => {
                let nid = NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: hwnd,
                    uID: TRAY_ICON_ID,
                    ..Default::default()
                };
                unsafe {
                    Shell_NotifyIconW(NIM_DELETE, &nid);
                    DestroyWindow(hwnd);
                }
                return 0;
            }
            WM_DESTROY => {
                unsafe {
                    PostQuitMessage(0);
                }
                return 0;
            }
            _ => {}
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn show_context_menu(hwnd: HWND) {
        let Some(shared) = SHARED.get() else {
            return;
        };
        let update_label = shared
            .update_label
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "Check for Updates…".to_string());

        unsafe {
            let hmenu = CreatePopupMenu();
            if hmenu.is_null() {
                return;
            }
            let open_w = wide("Open Nod");
            let upd_w = wide(&update_label);
            let quit_w = wide("Quit Nod");
            AppendMenuW(hmenu, MF_STRING, IDM_OPEN, open_w.as_ptr());
            AppendMenuW(hmenu, MF_STRING, IDM_UPDATE, upd_w.as_ptr());
            AppendMenuW(hmenu, MF_SEPARATOR, 0, null());
            AppendMenuW(hmenu, MF_STRING, IDM_QUIT, quit_w.as_ptr());

            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            SetForegroundWindow(hwnd);
            TrackPopupMenu(
                hmenu,
                TPM_LEFTALIGN | TPM_BOTTOMALIGN,
                pt.x,
                pt.y,
                0,
                hwnd,
                null(),
            );
            DestroyMenu(hmenu);
        }
    }

    pub struct TrayController {
        hwnd_raw: usize,
        shared: Arc<TrayShared>,
    }

    impl TrayController {
        pub fn start<F>(on_action: F) -> Option<Self>
        where
            F: Fn(TrayAction) + Send + Sync + 'static,
        {
            let shared = SHARED
                .get_or_init(|| {
                    Arc::new(TrayShared {
                        callback: Arc::new(on_action),
                        update_label: Mutex::new("Check for Updates…".to_string()),
                        update_ready: AtomicBool::new(false),
                    })
                })
                .clone();

            let (tx, rx) = std::sync::mpsc::channel::<Option<usize>>();
            thread::spawn(move || unsafe {
                let cls_name = wide("NodTrayMessageWindow");
                let wc = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(wnd_proc),
                    lpszClassName: cls_name.as_ptr(),
                    ..std::mem::zeroed()
                };
                RegisterClassExW(&wc);
                let title_w = wide("Nod Tray");
                let hwnd = CreateWindowExW(
                    0,
                    cls_name.as_ptr(),
                    title_w.as_ptr(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    null(),
                );
                if hwnd.is_null() {
                    let _ = tx.send(None);
                    return;
                }
                let hicon = load_tray_icon() as *mut core::ffi::c_void;
                let mut nid = NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: hwnd,
                    uID: TRAY_ICON_ID,
                    uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
                    uCallbackMessage: WM_TRAY,
                    hIcon: hicon,
                    ..Default::default()
                };
                copy_wide_fixed(&mut nid.szTip, "Nod");
                Shell_NotifyIconW(NIM_ADD, &nid);
                let _ = tx.send(Some(hwnd as usize));

                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if !hicon.is_null() {
                    DestroyIcon(hicon);
                }
            });

            rx.recv()
                .ok()
                .flatten()
                .map(|hwnd_raw| Self { hwnd_raw, shared })
        }

        pub fn set_update_state(&self, label: &str, ready: bool) {
            if let Ok(mut g) = self.shared.update_label.lock() {
                *g = label.to_string();
            }
            self.shared.update_ready.store(ready, Ordering::SeqCst);
            if self.hwnd_raw != 0 {
                unsafe {
                    PostMessageW(self.hwnd_raw as HWND, WM_UPDATE_LABEL, 0, 0);
                }
            }
        }

        pub fn show_notification(&self, title: &str, body: &str) {
            if self.hwnd_raw == 0 {
                return;
            }
            let slot = BALLOON_PENDING.get_or_init(|| Mutex::new(None));
            if let Ok(mut g) = slot.lock() {
                *g = Some((title.to_string(), body.to_string()));
            }
            unsafe {
                PostMessageW(self.hwnd_raw as HWND, WM_SHOW_BALLOON, 0, 0);
            }
        }

        pub fn shutdown(&self) {
            if self.hwnd_raw != 0 {
                unsafe {
                    PostMessageW(self.hwnd_raw as HWND, WM_CLOSE, 0, 0);
                }
            }
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::TrayAction;

    pub struct TrayController;

    impl TrayController {
        pub fn start<F>(_on_action: F) -> Option<Self>
        where
            F: Fn(TrayAction) + Send + Sync + 'static,
        {
            None
        }

        pub fn set_update_state(&self, _label: &str, _ready: bool) {}

        pub fn show_notification(&self, _title: &str, _body: &str) {}

        pub fn shutdown(&self) {}
    }
}

pub use platform::TrayController;
