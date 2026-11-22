//! Windows system tray integration for the main window.
//!
//! Closing the main window (`×`, `Alt+F4`, or `Ctrl+W`) hides it instead of
//! ending the UI process; left-clicking or double-clicking the tray icon (or
//! re-opening Zeron) restores it, and the tray context menu provides Show and
//! Quit actions. Programmatic `WindowHandle::close()` / `WindowFactory::quit()`
//! calls bypass `on_close_requested`, so updater restarts and relaunches exit
//! cleanly.

use std::sync::{Mutex, mpsc};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GWLP_USERDATA, GetCursorPos, GetMessageW, GetWindowLongPtrW, HICON,
    IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTSIZE, LR_SHARED, LoadIconW, LoadImageW, MF_SEPARATOR,
    MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW,
    SetForegroundWindow, SetWindowLongPtrW, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_NONOTIFY,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_CLOSE,
    WM_CONTEXTMENU, WM_DESTROY, WM_ENDSESSION, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL,
    WM_RBUTTONUP, WNDCLASSEXW, WS_OVERLAPPED,
};
use zgx_desktop::{WindowContext, WindowHandle};

const WM_TRAY: u32 = WM_APP + 1;
const ID_SHOW: usize = 1001;
const ID_QUIT: usize = 1002;
const LANG_CHINESE_PRIMARY: u16 = 0x04;

static MAIN_WINDOW: Mutex<Option<WindowHandle>> = Mutex::new(None);

struct TrayState {
    nid: NOTIFYICONDATAW,
    taskbar_created: u32,
    quit: Box<dyn Fn() + Send>,
}

/// Restore and focus the main window if it is still open (possibly hidden in
/// the tray).
pub fn show_main() {
    let handle = MAIN_WINDOW
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().cloned());
    if let Some(handle) = handle {
        restore(&handle);
    }
}

fn restore(handle: &WindowHandle) {
    handle.set_minimized(false);
    handle.set_visible(true);
    handle.request_focus();
}

/// Install close-to-tray behavior and the tray icon for the main window.
pub fn install(cx: &mut WindowContext) {
    if cfg!(test) || std::env::var_os("ZGX_OFFSCREEN").is_some() {
        return;
    }

    if let Ok(mut slot) = MAIN_WINDOW.lock() {
        *slot = Some(cx.window.clone());
    }

    let hide = cx.window.clone();
    cx.on_close_requested(move || {
        hide.set_visible(false);
        false
    });

    let (quit_tx, mut quit_rx) = zgx::task::latest::<()>();
    let close_main = cx.window.clone();
    let quit_app = cx.windows.clone();
    cx.tasks.spawn(async move {
        if quit_rx.recv().await.is_some() {
            // Close the main window first so `on_closed` saves state and
            // tears down the tray icon before the event loop exits.
            close_main.close();
            quit_app.quit();
        }
    });

    let (ready_tx, ready_rx) = mpsc::sync_channel::<usize>(1);
    let tray_thread = std::thread::Builder::new()
        .name("zeron-tray".into())
        .spawn(move || {
            run_tray(
                ready_tx,
                Box::new(move || {
                    quit_tx.send(());
                }),
            );
        })
        .ok();

    let tray_hwnd = tray_thread
        .as_ref()
        .and_then(|_| ready_rx.recv().ok())
        .unwrap_or(0);

    let mut tray_thread = tray_thread;
    cx.on_closed(move || {
        if let Ok(mut slot) = MAIN_WINDOW.lock() {
            *slot = None;
        }
        if tray_hwnd != 0 {
            // SAFETY: `tray_hwnd` is the tray helper window owned by `tray_thread`,
            // which exits its message loop on `WM_CLOSE` -> `DestroyWindow` -> `PostQuitMessage`.
            unsafe {
                PostMessageW(tray_hwnd as HWND, WM_CLOSE, 0, 0);
            }
        }
        if let Some(handle) = tray_thread.take() {
            let _ = handle.join();
        }
    });
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn write_tip(dst: &mut [u16; 128], tip: &str) {
    dst.fill(0);
    for (slot, code_unit) in dst.iter_mut().take(127).zip(tip.encode_utf16()) {
        *slot = code_unit;
    }
}

fn prefers_chinese() -> bool {
    // SAFETY: `GetUserDefaultUILanguage` takes no pointers and returns a LANGID.
    let lang = unsafe { GetUserDefaultUILanguage() };
    (lang & 0x03ff) == LANG_CHINESE_PRIMARY
}

fn run_tray(ready_tx: mpsc::SyncSender<usize>, quit: Box<dyn Fn() + Send>) {
    let class_name = wide("ZeronWindowsTrayClass");
    let window_name = wide("ZeronTrayWindow");
    let taskbar_msg = wide("TaskbarCreated");

    // SAFETY: `GetModuleHandleW(null)` returns the current executable's HMODULE.
    let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };

    // SAFETY: `WNDCLASSEXW` is a plain C struct; setting `cbSize`, `lpfnWndProc`,
    // `hInstance`, and NUL-terminated `lpszClassName` satisfies `RegisterClassExW`.
    unsafe {
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = hinstance;
        wc.lpszClassName = class_name.as_ptr();
        let _ = RegisterClassExW(&wc);
    }

    // Top-level hidden window (never `HWND_MESSAGE`) so Windows delivers
    // the `"TaskbarCreated"` broadcast if `explorer.exe` restarts.
    // SAFETY: NUL-terminated strings and valid `hinstance`.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_name.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        let _ = ready_tx.send(0);
        return;
    }

    // SAFETY: resource ID 1 is embedded by `build.rs` (`dist/windows/zeron.rc`),
    // falling back to the system application icon; `LR_SHARED` keeps ownership with Windows.
    let mut icon = unsafe {
        LoadImageW(
            hinstance,
            1usize as *const u16,
            IMAGE_ICON,
            0,
            0,
            LR_DEFAULTSIZE | LR_SHARED,
        ) as HICON
    };
    if icon.is_null() {
        // SAFETY: `IDI_APPLICATION` is a built-in shared system icon.
        icon = unsafe { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) };
    }

    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    write_tip(&mut nid.szTip, "Zeron");

    // SAFETY: NUL-terminated UTF-16 message name.
    let taskbar_created = unsafe { RegisterWindowMessageW(taskbar_msg.as_ptr()) };

    let state_ptr = Box::into_raw(Box::new(TrayState {
        nid,
        taskbar_created,
        quit,
    }));

    // SAFETY: `hwnd` belongs to this thread; `state_ptr` remains alive until
    // after `GetMessageW` exits and `WM_DESTROY` clears `GWLP_USERDATA`.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
        Shell_NotifyIconW(NIM_ADD, &(*state_ptr).nid);
    }

    let _ = ready_tx.send(hwnd as usize);

    // SAFETY: standard Win32 message pump for `hwnd` on this thread.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Box::from_raw(state_ptr));
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: `GWLP_USERDATA` is either 0 or the live `TrayState` owned by `run_tray` on this thread.
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut TrayState;
    if !ptr.is_null() {
        // SAFETY: `ptr` is valid on this thread until `WM_DESTROY` clears `GWLP_USERDATA`.
        let state = unsafe { &*ptr };
        if state.taskbar_created != 0 && msg == state.taskbar_created {
            // SAFETY: re-registers the tray icon after Explorer restarts.
            unsafe {
                Shell_NotifyIconW(NIM_ADD, &state.nid);
            }
            return 0;
        }
        match msg {
            WM_TRAY => {
                match lparam as u32 {
                    WM_LBUTTONUP | WM_LBUTTONDBLCLK => show_main(),
                    WM_RBUTTONUP | WM_CONTEXTMENU => {
                        // SAFETY: `hwnd` and `state` are live on this thread.
                        unsafe { show_context_menu(hwnd, state) };
                    }
                    _ => {}
                }
                return 0;
            }
            WM_ENDSESSION => {
                if wparam != 0 {
                    (state.quit)();
                }
                return 0;
            }
            WM_CLOSE => {
                // SAFETY: destroys the tray window on its owning thread.
                unsafe {
                    DestroyWindow(hwnd);
                }
                return 0;
            }
            WM_DESTROY => {
                // SAFETY: removes the tray icon and ends the thread's message loop.
                unsafe {
                    Shell_NotifyIconW(NIM_DELETE, &state.nid);
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    PostQuitMessage(0);
                }
                return 0;
            }
            _ => {}
        }
    }
    // SAFETY: forwards unhandled messages to the default window procedure.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

unsafe fn show_context_menu(hwnd: HWND, state: &TrayState) {
    // SAFETY: creates a transient popup menu, displays it at the cursor, and destroys it.
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let (show_label, quit_label) = if prefers_chinese() {
            (wide("显示 Zeron"), wide("退出 Zeron"))
        } else {
            (wide("Show Zeron"), wide("Quit Zeron"))
        };
        AppendMenuW(menu, MF_STRING, ID_SHOW, show_label.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, ID_QUIT, quit_label.as_ptr());

        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        // KB135788: focus the owner window before `TrackPopupMenu` and post
        // `WM_NULL` afterward so clicking outside dismisses the menu cleanly.
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN | TPM_NONOTIFY | TPM_RETURNCMD,
            pt.x,
            pt.y,
            0,
            hwnd,
            std::ptr::null(),
        ) as usize;
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);

        if cmd == ID_SHOW {
            show_main();
        } else if cmd == ID_QUIT {
            (state.quit)();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_tip_is_nul_terminated_and_bounded() {
        let mut tip = [1u16; 128];
        write_tip(&mut tip, "Zeron");
        assert_eq!(&tip[..6], &['Z' as u16, 'e' as u16, 'r' as u16, 'o' as u16, 'n' as u16, 0]);
        assert!(tip[6..].iter().all(|&u| u == 0));

        let long = "a".repeat(200);
        write_tip(&mut tip, &long);
        assert_eq!(tip[127], 0);
        assert!(tip[..127].iter().all(|&u| u == 'a' as u16));
    }

    #[test]
    fn wide_string_ends_with_nul() {
        let w = wide("退出 Zeron");
        assert_eq!(w.last().copied(), Some(0));
    }
}
