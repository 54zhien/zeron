//! Windows system tray integration for the main window.
//!
//! Closing the main window (`×`, `Alt+F4`, or `Ctrl+W`) hides it instead of
//! ending the UI process; left-clicking or double-clicking the tray icon (or
//! re-opening Zeron) restores it, and right-clicking opens a Zeron-styled
//! frosted-glass menu window (`Show Zeron` / `Quit Zeron`) driven by the same
//! `zgx` theme, material, and locale state as the main GUI.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW, GWL_EXSTYLE,
    GWLP_USERDATA, GetCursorPos, GetMessageW, GetWindowLongPtrW, GetWindowThreadProcessId, HICON,
    IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTSIZE, LR_SHARED, LoadIconW, LoadImageW, MSG,
    PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, TranslateMessage, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_ENDSESSION,
    WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSEXW, WS_EX_APPWINDOW,
    WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};
use zgx::compose::{button, column, component, div, row, svg, switch, text};
use zgx::cursor::Cursor;
use zgx::input::{InputEvent, Key};
use zgx::reactive::Signal;
use zgx::scene::Color;
use zgx::style::Styled;
use zgx::svg::SvgData;
use zgx::text_layout::FontFamily;
use zgx::theme::{Appearance, ThemeScope, Themes, TokenValue, token};
use zgx_desktop::{
    Glass, GlassMaterial, Titlebar, WindowBounds, WindowContext, WindowFactory, WindowHandle,
    WindowLevel, WindowOptions,
};
use zgx_script::Value;

const WM_TRAY: u32 = WM_APP + 1;
const LANG_CHINESE_PRIMARY: u16 = 0x04;

const MENU_TITLE: &str = "ZeronTrayMenu";
const MENU_WIDTH: f64 = 196.0;
// 1 (top border) + 4 (pad) + 22 (brand) + 2 (gap) + 28 (Show) + 4 (pad)
// + 1 (rule) + 4 (pad) + 28 (Quit) + 4 (pad) + 1 (bottom border) = 99
const MENU_HEIGHT: f64 = 99.0;
const EDGE_MARGIN: f64 = 8.0;
const CURSOR_GAP: f64 = 6.0;

const LOGO_SVG: &str = include_str!("../ui/assets/icons/ui/zeron-logo.svg");
const SHOW_SVG: &str = include_str!("../ui/assets/icons/ui/monitor.svg");
const QUIT_SVG: &str = include_str!("../ui/assets/icons/ui/close-circle.svg");

static MAIN_WINDOW: Mutex<Option<WindowHandle>> = Mutex::new(None);
static MENU_WINDOW: Mutex<Option<WindowHandle>> = Mutex::new(None);

struct TrayState {
    nid: NOTIFYICONDATAW,
    taskbar_created: u32,
    open_menu: Box<dyn Fn(i32, i32) + Send>,
    quit: Box<dyn Fn() + Send>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TrayPalette {
    dark: bool,
    frosted: bool,
    surface: Color,
    border: Color,
    rule: Color,
    text_primary: Color,
    text_secondary: Color,
    text_tertiary: Color,
    hover: Color,
    pressed: Color,
    danger: Color,
    danger_hover: Color,
    danger_pressed: Color,
}

impl TrayPalette {
    fn from_theme(themes: &Themes) -> Self {
        themes.current();
        let dark = themes.kind().is_dark();
        let root = themes.root();
        let frosted = match root.get(token("zeron.island.blur")) {
            TokenValue::Number(blur) => blur > 0.0,
            _ => true,
        };

        let surface = if frosted {
            let tint = tok_first_color(
                root,
                &["zeron.menu.tint", "surface.overlay", "glass.tint"],
                if dark {
                    Color(0x14, 0x14, 0x17, 0xc8)
                } else {
                    Color(0xf6, 0xf6, 0xf8, 0xcc)
                },
            );
            let max_alpha = if dark { 0xc8 } else { 0xcc };
            Color(tint.0, tint.1, tint.2, tint.3.min(max_alpha))
        } else {
            let solid = tok_first_color(
                root,
                &["zeron.menu.solid", "zeron.window", "surface.overlay"],
                if dark {
                    Color(0x1c, 0x1c, 0x1f, 0xff)
                } else {
                    Color(0xfb, 0xfb, 0xfc, 0xff)
                },
            );
            Color(solid.0, solid.1, solid.2, 0xff)
        };

        let border = tok_color(
            root,
            "border",
            if dark {
                Color(0xff, 0xff, 0xff, 0x18)
            } else {
                Color(0, 0, 0, 0x24)
            },
        );
        let rule = tok_color(
            root,
            "border.subtle",
            if dark {
                Color(0xff, 0xff, 0xff, 0x0c)
            } else {
                Color(0, 0, 0, 0x17)
            },
        );
        let text_primary = tok_color(
            root,
            "text.primary",
            if dark {
                Color(0xed, 0xed, 0xef, 0xff)
            } else {
                Color(0x1b, 0x1b, 0x1d, 0xff)
            },
        );
        let text_secondary = tok_color(
            root,
            "text.secondary",
            if dark {
                Color(0xed, 0xed, 0xef, 153)
            } else {
                Color(0x1b, 0x1b, 0x1d, 163)
            },
        );
        let text_tertiary = tok_color(
            root,
            "text.tertiary",
            if dark {
                Color(0xed, 0xed, 0xef, 107)
            } else {
                Color(0x1b, 0x1b, 0x1d, 115)
            },
        );
        let hover = tok_color(
            root,
            "hover",
            if dark {
                Color(0xff, 0xff, 0xff, 0x0d)
            } else {
                Color(0, 0, 0, 0x0a)
            },
        );
        let pressed = tok_color(
            root,
            "pressed",
            if dark {
                Color(0xff, 0xff, 0xff, 0x17)
            } else {
                Color(0, 0, 0, 0x12)
            },
        );
        let danger = tok_color(
            root,
            "danger",
            if dark {
                Color(0xf8, 0x71, 0x71, 0xff)
            } else {
                Color(0xdc, 0x26, 0x26, 0xff)
            },
        );
        let danger_hover = tok_color(
            root,
            "zeron.menu.danger",
            Color(danger.0, danger.1, danger.2, 36),
        );
        let danger_pressed = Color(danger.0, danger.1, danger.2, 56);

        Self {
            dark,
            frosted,
            surface,
            border,
            rule,
            text_primary,
            text_secondary,
            text_tertiary,
            hover,
            pressed,
            danger,
            danger_hover,
            danger_pressed,
        }
    }
}

fn tok_color(root: &ThemeScope, name: &str, fallback: Color) -> Color {
    root.get(token(name)).color().unwrap_or(fallback)
}

fn tok_first_color(root: &ThemeScope, names: &[&str], fallback: Color) -> Color {
    for &name in names {
        if let Some(color) = root.get(token(name)).color() {
            return color;
        }
    }
    fallback
}

fn is_chinese(locale: &Signal<Value>) -> bool {
    locale.with(|value| match value {
        Value::Str(s) if !s.is_empty() => s.starts_with("zh"),
        Value::Text(s) => {
            let text = s.to_string();
            if text.is_empty() {
                prefers_chinese()
            } else {
                text.starts_with("zh")
            }
        }
        _ => prefers_chinese(),
    })
}

/// Hide the tray popup menu window if it is currently open.
pub fn hide_menu() {
    let handle = MENU_WINDOW
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().cloned());
    if let Some(handle) = handle {
        handle.set_visible(false);
    }
}

/// Restore and focus the main window if it is still open (possibly hidden in
/// the tray).
pub fn show_main() {
    hide_menu();
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

/// Install close-to-tray behavior, the tray icon, and the Zeron-styled tray
/// context menu window for the main window.
pub fn install(cx: &mut WindowContext, locale: Signal<Value>) {
    if cfg!(test) || std::env::var_os("ZGX_OFFSCREEN").is_some() {
        return;
    }

    if let Ok(mut slot) = MAIN_WINDOW.lock() {
        *slot = Some(cx.window.clone());
    }

    let hide = cx.window.clone();
    cx.on_close_requested(move || {
        hide_menu();
        hide.set_visible(false);
        false
    });

    let (quit_tx, mut quit_rx) = zgx::task::latest::<()>();
    let close_main = cx.window.clone();
    let quit_app = cx.windows.clone();
    cx.tasks.spawn(async move {
        if quit_rx.recv().await.is_some() {
            // Close the main window first so `on_closed` saves state and
            // tears down the tray icon and menu window before exiting.
            close_main.close();
            quit_app.quit();
        }
    });

    let menu_slot: Rc<RefCell<Option<WindowHandle>>> = Rc::new(RefCell::new(None));
    let ensure_menu = {
        let menu_slot = menu_slot.clone();
        let windows = cx.windows.clone();
        let close_main = cx.window.clone();
        let quit_app = cx.windows.clone();
        let locale = locale.clone();
        Rc::new(move || -> WindowHandle {
            if let Some(existing) = menu_slot.borrow().as_ref().filter(|h| !h.is_closed()) {
                return existing.clone();
            }
            let handle = open_menu_window(&windows, close_main.clone(), quit_app.clone(), locale.clone());
            *menu_slot.borrow_mut() = Some(handle.clone());
            handle
        })
    };

    // Pre-warm the menu window shortly after launch so the first right-click opens immediately.
    let prewarm = ensure_menu.clone();
    cx.tasks.spawn(async move {
        zgx::timer::sleep(Duration::from_millis(400)).await;
        let _ = prewarm();
    });

    let (menu_tx, mut menu_rx) = zgx::task::latest::<(i32, i32)>();
    let open_on_click = ensure_menu;
    let displays = cx.window_info.displays.clone();
    cx.tasks.spawn(async move {
        while let Some((x, y)) = menu_rx.recv().await {
            let handle = open_on_click();
            apply_tool_window_style();
            let work = work_area_at(x, y);
            let scale = scale_at(&displays, x, y);
            let bounds = place_menu((x, y), work, scale);
            handle.set_bounds(bounds);
            handle.set_visible(true);
            handle.request_focus();
        }
    });

    let (ready_tx, ready_rx) = mpsc::sync_channel::<usize>(1);
    let tray_thread = std::thread::Builder::new()
        .name("zeron-tray".into())
        .spawn(move || {
            run_tray(
                ready_tx,
                Box::new(move |x, y| {
                    menu_tx.send((x, y));
                }),
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
        let menu = MENU_WINDOW.lock().ok().and_then(|mut slot| slot.take());
        if let Some(menu) = menu {
            menu.close();
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

fn open_menu_window(
    windows: &WindowFactory,
    close_main: WindowHandle,
    quit_app: WindowFactory,
    locale: Signal<Value>,
) -> WindowHandle {
    let options = WindowOptions {
        title: MENU_TITLE.into(),
        width: MENU_WIDTH,
        height: MENU_HEIGHT,
        transparent: true,
        resizable: false,
        decorations: false,
        titlebar: Titlebar::Hidden,
        glass: Some(Glass::new(GlassMaterial::UnderWindowBackground).plain()),
        level: WindowLevel::AlwaysOnTop,
        shadow: true,
        visible: false,
        ..Default::default()
    };
    windows.open(options, move |cx| {
        if let Ok(mut slot) = MENU_WINDOW.lock() {
            *slot = Some(cx.window.clone());
        }
        let hide_on_close = cx.window.clone();
        cx.on_close_requested(move || {
            hide_on_close.set_visible(false);
            false
        });
        cx.on_closed(|| {
            if let Ok(mut slot) = MENU_WINDOW.lock() {
                *slot = None;
            }
        });

        let ready = cx.window_info.ready.clone();
        cx.ui.runtime.effect(move || {
            if ready.get() {
                apply_tool_window_style();
            }
        });

        let active = cx.ui.observe_window_active();
        let was_active = Rc::new(Cell::new(false));
        let hide_on_blur = cx.window.clone();
        cx.ui.runtime.effect(move || {
            if active.get() {
                was_active.set(true);
            } else if was_active.replace(false) {
                hide_on_blur.set_visible(false);
            }
        });

        let themes = Themes::of(&cx.ui.runtime);
        let sync_chrome_themes = themes.clone();
        let sync_chrome_window = cx.window.clone();
        let applied_chrome = Rc::new(Cell::new(None::<(bool, bool)>));
        cx.ui.runtime.effect(move || {
            let palette = TrayPalette::from_theme(&sync_chrome_themes);
            let next = (palette.dark, palette.frosted);
            if applied_chrome.get() != Some(next) {
                applied_chrome.set(Some(next));
                sync_chrome_window.set_theme(Some(if palette.dark {
                    Appearance::Dark
                } else {
                    Appearance::Light
                }));
                sync_chrome_window.set_glass(
                    palette
                        .frosted
                        .then(|| Glass::new(GlassMaterial::UnderWindowBackground).plain()),
                );
            }
        });

        let view_themes = themes;
        let view_locale = locale.clone();
        let hide_on_escape = cx.window.clone();
        cx.render(component(move |_| {
            let themes = view_themes.clone();
            let locale = view_locale.clone();
            let close_main = close_main.clone();
            let quit_app = quit_app.clone();
            let hide_on_escape = hide_on_escape.clone();

            switch(
                move || (TrayPalette::from_theme(&themes), is_chinese(&locale)),
                move |(palette, zh), _| {
                    let close_main = close_main.clone();
                    let quit_app = quit_app.clone();
                    let hide_on_escape = hide_on_escape.clone();
                    let show_label = if zh { "显示 Zeron" } else { "Show Zeron" };
                    let quit_label = if zh { "退出 Zeron" } else { "Quit Zeron" };

                    column()
                        .w_full()
                        .h_full()
                        .rounded(8.)
                        .border(1.)
                        .border_color(palette.border)
                        .bg(palette.surface)
                        .on_document_event(move |event| {
                            if matches!(
                                event,
                                InputEvent::KeyDown {
                                    key: Key::Escape,
                                    ..
                                }
                            ) {
                                hide_on_escape.set_visible(false);
                            }
                        })
                        .child(
                            column()
                                .w_full()
                                .p(4.)
                                .gap(2.)
                                .child(
                                    row()
                                        .w_full()
                                        .h(22.)
                                        .px(8.)
                                        .gap(6.)
                                        .items_center()
                                        .child(tinted_svg("", LOGO_SVG, 12., palette.text_tertiary))
                                        .child(
                                            text("Zeron")
                                                .font_family(FontFamily::from("Geist"))
                                                .text_size(11.)
                                                .line_height(14.)
                                                .font_weight(500)
                                                .text_color(palette.text_tertiary)
                                                .text_wrap(false),
                                        )
                                        .child(div().grow())
                                        .child(
                                            text(format!("v{}", crate::about::APP_VERSION))
                                                .font_family(FontFamily::from("Geist"))
                                                .text_size(10.5)
                                                .line_height(14.)
                                                .text_color(palette.text_tertiary)
                                                .text_wrap(false),
                                        ),
                                )
                                .child(menu_row(
                                    show_label,
                                    SHOW_SVG,
                                    palette.text_secondary,
                                    palette.text_primary,
                                    palette.hover,
                                    palette.pressed,
                                    || {
                                        show_main();
                                    },
                                )),
                        )
                        .child(div().w_full().h(1.).bg(palette.rule))
                        .child(
                            column()
                                .w_full()
                                .p(4.)
                                .gap(2.)
                                .child(menu_row(
                                    quit_label,
                                    QUIT_SVG,
                                    palette.danger,
                                    palette.danger,
                                    palette.danger_hover,
                                    palette.danger_pressed,
                                    move || {
                                        hide_menu();
                                        close_main.close();
                                        quit_app.quit();
                                    },
                                )),
                        )
                },
            )
        }));
    })
}

fn tinted_svg(label: &str, source: &'static str, size: f32, color: Color) -> zgx::compose::View {
    let data = SvgData::new(source.as_bytes())
        .expect("tray svg icon")
        .tinted(color);
    svg(label, Arc::new(data)).size(size, size).shrink_0()
}

fn menu_row(
    label: &'static str,
    icon_svg: &'static str,
    icon_color: Color,
    text_color: Color,
    hover_bg: Color,
    pressed_bg: Color,
    on_click: impl FnMut() + 'static,
) -> zgx::compose::View {
    button()
        .accessible_label(label)
        .w_full()
        .h(28.)
        .px(8.)
        .gap(8.)
        .items_center()
        .rounded(5.)
        .bg(Color(0, 0, 0, 0))
        .border(0.)
        .cursor(Cursor::Pointer)
        .hover(move |s| s.bg(hover_bg))
        .active(move |s| s.bg(pressed_bg))
        .on_click(on_click)
        .child(
            div()
                .size(18., 18.)
                .shrink_0()
                .items_center()
                .justify_center()
                .child(tinted_svg("", icon_svg, 16., icon_color)),
        )
        .child(
            text(label)
                .font_family(FontFamily::from("Geist"))
                .text_size(13.)
                .line_height(18.)
                .text_color(text_color)
                .text_wrap(false),
        )
}

fn apply_tool_window_style() {
    let title = wide(MENU_TITLE);
    // SAFETY: `title` is a NUL-terminated UTF-16 string; we verify the found
    // window belongs to the current process before altering its extended style.
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if hwnd.is_null() {
            return;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != std::process::id() {
            return;
        }
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let next = (ex_style | WS_EX_TOOLWINDOW) & !WS_EX_APPWINDOW;
        if next != ex_style {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next as isize);
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }
}

fn work_area_at(x: i32, y: i32) -> (i32, i32, i32, i32) {
    // SAFETY: `MonitorFromPoint` with `MONITOR_DEFAULTTONEAREST` always returns
    // a monitor handle, and `MONITORINFO` has `cbSize` properly initialized.
    unsafe {
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        if GetMonitorInfoW(monitor, &mut info) != 0 {
            let r = info.rcWork;
            (r.left, r.top, r.right, r.bottom)
        } else {
            (0, 0, 1920, 1080)
        }
    }
}

fn scale_at(displays: &Signal<Vec<zgx_desktop::DisplayInfo>>, x: i32, y: i32) -> f64 {
    displays.with_untracked(|list| {
        list.iter()
            .find(|d| {
                let (dx, dy) = d.position;
                let (dw, dh) = (d.size.0 as i32, d.size.1 as i32);
                x >= dx && x < dx + dw && y >= dy && y < dy + dh
            })
            .or_else(|| list.iter().find(|d| d.primary))
            .or_else(|| list.first())
            .map(|d| d.scale_factor.max(1.0))
            .unwrap_or(1.0)
    })
}

fn place_menu(cursor: (i32, i32), work: (i32, i32, i32, i32), scale: f64) -> WindowBounds {
    let scale = if scale.is_finite() && scale >= 1.0 {
        scale
    } else {
        1.0
    };
    let w = (MENU_WIDTH * scale).round().max(1.0) as i32;
    let h = (MENU_HEIGHT * scale).round().max(1.0) as i32;
    let margin = (EDGE_MARGIN * scale).round() as i32;
    let gap = (CURSOR_GAP * scale).round() as i32;

    let (left, top, right, bottom) = work;
    let min_x = left + margin;
    let max_x = (right - margin - w).max(min_x);
    let x = cursor.0.clamp(min_x, max_x);

    let min_y = top + margin;
    let max_y = (bottom - margin - h).max(min_y);
    let prefer_above = cursor.1 + gap + h > bottom - margin && cursor.1 - gap - h >= min_y;
    let raw_y = if prefer_above {
        cursor.1 - gap - h
    } else {
        cursor.1 + gap
    };
    let y = raw_y.clamp(min_y, max_y);

    WindowBounds::new(w as u32, h as u32).at(x, y)
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

fn run_tray(
    ready_tx: mpsc::SyncSender<usize>,
    open_menu: Box<dyn Fn(i32, i32) + Send>,
    quit: Box<dyn Fn() + Send>,
) {
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
        open_menu,
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
                        let mut pt = POINT { x: 0, y: 0 };
                        // SAFETY: `pt` is valid stack storage; `SetForegroundWindow` transfers
                        // shell tray click foreground rights to this process for `request_focus`.
                        unsafe {
                            GetCursorPos(&mut pt);
                            SetForegroundWindow(hwnd);
                        }
                        (state.open_menu)(pt.x, pt.y);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_tip_is_nul_terminated_and_bounded() {
        let mut tip = [1u16; 128];
        write_tip(&mut tip, "Zeron");
        assert_eq!(
            &tip[..6],
            &['Z' as u16, 'e' as u16, 'r' as u16, 'o' as u16, 'n' as u16, 0]
        );
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

    #[test]
    fn place_menu_bottom_right_taskbar_100_and_150_dpi() {
        let bounds_1x = place_menu((1850, 1056), (0, 0, 1920, 1032), 1.0);
        assert_eq!(bounds_1x.size, (196, 99));
        assert_eq!(bounds_1x.position, Some((1716, 925)));

        let bounds_15x = place_menu((2800, 1580), (0, 0, 2880, 1548), 1.5);
        assert_eq!(bounds_15x.size, (294, 149));
        assert_eq!(bounds_15x.position, Some((2574, 1387)));
    }

    #[test]
    fn place_menu_top_taskbar_and_negative_monitor() {
        let top_bar = place_menu((1850, 24), (0, 48, 1920, 1080), 1.0);
        assert_eq!(top_bar.position, Some((1716, 56)));

        let neg_monitor = place_menu((-1800, 1050), (-1920, 0, 0, 1032), 1.0);
        assert_eq!(neg_monitor.position, Some((-1800, 925)));
    }
}
