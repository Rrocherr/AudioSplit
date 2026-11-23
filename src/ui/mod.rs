mod tray;

use std::ffi::c_void;
use std::sync::Arc;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, GetSysColor, GetSysColorBrush, SetBkMode, SetTextColor,
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, COLOR_BTNFACE, COLOR_GRAYTEXT, COLOR_WINDOW,
    COLOR_WINDOWTEXT, DEFAULT_CHARSET, DEFAULT_PITCH, FF_DONTCARE, HBRUSH, HDC, HFONT,
    OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemServices::{SS_CENTERIMAGE, SS_ENDELLIPSIS, SS_LEFT, SS_RIGHT};
use windows::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, EM_SETLIMITTEXT, SetScrollInfo, TBM_SETPOS, TBM_SETRANGE,
    TBS_BOTH, TBS_HORZ, TBS_NOTICKS, TRACKBAR_CLASSW, UDM_SETBUDDY, UDM_SETPOS32, UDM_SETRANGE32,
    UDS_ALIGNRIGHT, UDS_ARROWKEYS, UDS_NOTHOUSANDS, UDS_SETBUDDYINT, UPDOWN_CLASSW,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow,
    DispatchMessageW, GetClientRect, GetMessageW, GetParent, GetScrollInfo, GetWindowLongPtrW,
    GetWindowLongW, GetWindowTextW, IsWindowVisible, KillTimer, LoadCursorW, MoveWindow,
    PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, ScrollWindowEx,
    SendMessageW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, SetWindowTextW,
    ShowWindow, TranslateMessage, BM_GETCHECK, BM_SETCHECK, BN_CLICKED, BS_AUTOCHECKBOX,
    BS_PUSHBUTTON, CS_HREDRAW, CS_VREDRAW, EN_CHANGE, ES_NUMBER, ES_RIGHT, GWLP_USERDATA,
    GWL_STYLE, HMENU, IDC_ARROW, MINMAXINFO, MSG, SB_LINEDOWN, SB_LINEUP, SB_PAGEDOWN, SB_PAGEUP,
    SB_THUMBPOSITION, SB_THUMBTRACK, SB_VERT, SCROLLINFO, SIF_ALL, SIF_PAGE, SIF_POS, SIF_RANGE,
    SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SCROLLCHILDREN, WM_APP,
    WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLORSTATIC, WM_DESTROY, WM_DPICHANGED,
    WM_GETMINMAXINFO, WM_HSCROLL, WM_LBUTTONUP, WM_MOUSEWHEEL, WM_RBUTTONUP, WM_SETFONT,
    WM_SIZE, WM_TIMER, WM_USER, WM_VSCROLL, WNDCLASSEXW, WS_BORDER, WS_CHILD, WS_CLIPCHILDREN,
    WS_DISABLED, WS_EX_CONTROLPARENT, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};

use crate::audio::{
    enumerate_render_devices, AudioDeviceInfo, AudioManager, DeviceNotificationWatcher, SyncStatus,
};
use crate::config::{AppConfig, MAX_DELAY_MS};
use tray::{create_app_icon, TrayIcon, IDM_TRAY_QUIT, IDM_TRAY_SHOW_HIDE, IDM_TRAY_TOGGLE_ROUTING};

pub const WINDOW_CLASS: PCWSTR = w!("AudioSplitMainWindow");
const SCROLL_CLASS: PCWSTR = w!("AudioSplitScrollPanel");

const WM_APP_DEVICES_CHANGED: u32 = WM_APP + 1;
const WM_APP_TRAY: u32 = WM_APP + 2;
const TBM_GETPOS: u32 = WM_USER;

const IDC_BTN_TOGGLE_ROUTING: usize = 1001;
const IDC_BTN_REFRESH: usize = 1002;
const IDC_CHK_START_MINIMIZED: usize = 1003;
const IDC_BTN_HIDE: usize = 1004;
const IDC_BTN_QUIT: usize = 1005;

const ROW_BASE_ID: usize = 2000;
const ROW_STRIDE: usize = 10;

const TIMER_DEBOUNCE_REFRESH: usize = 1;
const TIMER_DEBOUNCE_SAVE: usize = 2;

#[inline]
fn scale(px: i32, dpi: u32) -> i32 {
    (px * dpi as i32 + 48) / 96
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

unsafe fn set_text(hwnd: HWND, text: &str) {
    let w = to_wide(text);
    let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
}

unsafe fn get_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let len = GetWindowTextW(hwnd, &mut buf);
    if len <= 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

unsafe fn set_font(hwnd: HWND, font: HFONT) {
    if !font.is_invalid() {
        let _ = SendMessageW(hwnd, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }
}

fn make_font(dpi: u32, bold: bool) -> HFONT {
    let height = -scale(12, dpi);
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            if bold { 600 } else { 400 },
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            w!("Segoe UI"),
        )
    }
}

struct DeviceRowControls {
    device_id: String,
    is_default: bool,
    checkbox: HWND,
    trackbar: Option<HWND>,
    vol_label: Option<HWND>,
    delay_edit: Option<HWND>,
    delay_updown: Option<HWND>,
    ms_label: Option<HWND>,
}

struct AppState {
    hwnd: HWND,
    hinst: HINSTANCE,
    dpi: u32,
    font_regular: HFONT,
    font_bold: HFONT,
    window_icon: windows::Win32::UI::WindowsAndMessaging::HICON,
    wm_taskbar_created: u32,

    // Static UI controls
    lbl_default: HWND,
    btn_toggle: HWND,
    lbl_status: HWND,
    btn_refresh: HWND,
    hdr_device: HWND,
    hdr_volume: HWND,
    hdr_delay: HWND,
    scroll_panel: HWND,
    chk_start_min: HWND,
    btn_hide: HWND,
    btn_quit: HWND,

    // Dynamic rows inside scroll_panel
    rows: Vec<DeviceRowControls>,
    scroll_y: i32,
    scroll_content_h: i32,

    // Core application state
    config: AppConfig,
    devices: Vec<AudioDeviceInfo>,
    default_id: Option<String>,
    audio: AudioManager,
    watcher: Option<DeviceNotificationWatcher>,
    tray: TrayIcon,
    suppress_events: bool,
}

impl AppState {
    fn new(hwnd: HWND, hinst: HINSTANCE) -> Box<Self> {
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        let hwnd_raw = hwnd.0 as isize;
        let on_lost = Arc::new(move || unsafe {
            let _ = PostMessageW(
                HWND(hwnd_raw as *mut c_void),
                WM_APP_DEVICES_CHANGED,
                WPARAM(0),
                LPARAM(0),
            );
        });

        let config = AppConfig::load();
        let tray = TrayIcon::new(hwnd, WM_APP_TRAY);
        let wm_taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        let window_icon = create_app_icon(true);

        Box::new(Self {
            hwnd,
            hinst,
            dpi,
            font_regular: make_font(dpi, false),
            font_bold: make_font(dpi, true),
            window_icon,
            wm_taskbar_created,
            lbl_default: HWND::default(),
            btn_toggle: HWND::default(),
            lbl_status: HWND::default(),
            btn_refresh: HWND::default(),
            hdr_device: HWND::default(),
            hdr_volume: HWND::default(),
            hdr_delay: HWND::default(),
            scroll_panel: HWND::default(),
            chk_start_min: HWND::default(),
            btn_hide: HWND::default(),
            btn_quit: HWND::default(),
            rows: Vec::new(),
            scroll_y: 0,
            scroll_content_h: 0,
            config,
            devices: Vec::new(),
            default_id: None,
            audio: AudioManager::new(on_lost),
            watcher: None,
            tray,
            suppress_events: false,
        })
    }

    unsafe fn create_child(
        &self,
        ex_style: u32,
        class: PCWSTR,
        text: &str,
        style: u32,
        id: usize,
        parent: HWND,
        bold: bool,
    ) -> HWND {
        let wide = to_wide(text);
        let h = CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(ex_style),
            class,
            PCWSTR(wide.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(style),
            0,
            0,
            10,
            10,
            parent,
            HMENU(id as *mut c_void),
            self.hinst,
            None,
        )
        .unwrap_or_default();
        set_font(h, if bold { self.font_bold } else { self.font_regular });
        h
    }

    unsafe fn init_controls(&mut self) {
        let base_static = WS_CHILD.0 | WS_VISIBLE.0 | SS_LEFT.0 | SS_CENTERIMAGE.0;
        let base_btn = WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32;

        self.lbl_default = self.create_child(
            0,
            w!("STATIC"),
            "Default source: Detecting...",
            WS_CHILD.0 | WS_VISIBLE.0 | SS_LEFT.0 | SS_CENTERIMAGE.0 | SS_ENDELLIPSIS.0,
            0,
            self.hwnd,
            true,
        );
        self.btn_toggle = self.create_child(
            0,
            w!("BUTTON"),
            "Pause Routing",
            base_btn,
            IDC_BTN_TOGGLE_ROUTING,
            self.hwnd,
            false,
        );
        self.lbl_status = self.create_child(
            0,
            w!("STATIC"),
            "Initializing...",
            WS_CHILD.0 | WS_VISIBLE.0 | SS_LEFT.0 | SS_CENTERIMAGE.0 | SS_ENDELLIPSIS.0,
            0,
            self.hwnd,
            false,
        );
        self.btn_refresh = self.create_child(
            0,
            w!("BUTTON"),
            "Refresh",
            base_btn,
            IDC_BTN_REFRESH,
            self.hwnd,
            false,
        );

        self.hdr_device =
            self.create_child(0, w!("STATIC"), "Output Device", base_static, 0, self.hwnd, true);
        self.hdr_volume =
            self.create_child(0, w!("STATIC"), "Volume", base_static, 0, self.hwnd, true);
        self.hdr_delay =
            self.create_child(0, w!("STATIC"), "Delay (ms)", base_static, 0, self.hwnd, true);

        self.scroll_panel = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            SCROLL_CLASS,
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_BORDER | WS_VSCROLL | WS_TABSTOP | WS_CLIPCHILDREN,
            0,
            0,
            100,
            100,
            self.hwnd,
            None,
            self.hinst,
            None,
        )
        .unwrap_or_default();

        self.chk_start_min = self.create_child(
            0,
            w!("BUTTON"),
            "Start minimized to tray",
            WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
            IDC_CHK_START_MINIMIZED,
            self.hwnd,
            false,
        );
        let check_state = if self.config.start_minimized {
            BST_CHECKED
        } else {
            BST_UNCHECKED
        };
        let _ = SendMessageW(self.chk_start_min, BM_SETCHECK, WPARAM(check_state.0 as usize), LPARAM(0));

        self.btn_hide = self.create_child(
            0,
            w!("BUTTON"),
            "Hide to Tray",
            base_btn,
            IDC_BTN_HIDE,
            self.hwnd,
            false,
        );
        self.btn_quit = self.create_child(
            0,
            w!("BUTTON"),
            "Quit",
            base_btn,
            IDC_BTN_QUIT,
            self.hwnd,
            false,
        );

        // Set initial window icon
        let _ = SendMessageW(
            self.hwnd,
            windows::Win32::UI::WindowsAndMessaging::WM_SETICON,
            WPARAM(0),
            LPARAM(self.window_icon.0 as isize),
        );
        let _ = SendMessageW(
            self.hwnd,
            windows::Win32::UI::WindowsAndMessaging::WM_SETICON,
            WPARAM(1),
            LPARAM(self.window_icon.0 as isize),
        );

        self.tray.add(false, "AudioSplit - Initializing");
        self.watcher = DeviceNotificationWatcher::register(self.hwnd, WM_APP_DEVICES_CHANGED).ok();

        self.layout_main();
        self.refresh_devices_and_sync();
    }

    unsafe fn layout_main(&mut self) {
        let mut rc = RECT::default();
        let _ = GetClientRect(self.hwnd, &mut rc);
        let cw = (rc.right - rc.left).max(1);
        let ch = (rc.bottom - rc.top).max(1);
        let d = self.dpi;

        let margin = scale(12, d);
        let top_h = scale(22, d);
        let btn_h = scale(26, d);
        let toggle_w = scale(122, d);
        let refresh_w = scale(82, d);

        let _ = MoveWindow(
            self.lbl_default,
            margin,
            scale(10, d),
            (cw - margin * 2).max(50),
            top_h,
            true,
        );

        let row2_y = scale(36, d);
        let _ = MoveWindow(self.btn_toggle, margin, row2_y, toggle_w, btn_h, true);
        let _ = MoveWindow(
            self.btn_refresh,
            cw - margin - refresh_w,
            row2_y,
            refresh_w,
            btn_h,
            true,
        );
        let status_x = margin + toggle_w + scale(10, d);
        let status_w = (cw - margin - refresh_w - scale(10, d) - status_x).max(40);
        let _ = MoveWindow(self.lbl_status, status_x, row2_y + scale(3, d), status_w, scale(20, d), true);

        let hdr_y = scale(70, d);
        let hdr_h = scale(18, d);
        let list_y = scale(90, d);
        let footer_h = scale(42, d);
        let list_h = (ch - list_y - footer_h).max(scale(80, d));
        let list_w = (cw - margin * 2).max(scale(200, d));

        let _ = MoveWindow(self.scroll_panel, margin, list_y, list_w, list_h, true);

        let mut panel_rc = RECT::default();
        let _ = GetClientRect(self.scroll_panel, &mut panel_rc);
        let panel_w = (panel_rc.right - panel_rc.left).max(100);
        let (dev_w, vol_x, delay_x) = self.row_columns(panel_w);

        let _ = MoveWindow(self.hdr_device, margin + scale(8, d), hdr_y, dev_w, hdr_h, true);
        let _ = MoveWindow(
            self.hdr_volume,
            margin + vol_x,
            hdr_y,
            scale(148, d),
            hdr_h,
            true,
        );
        let _ = MoveWindow(
            self.hdr_delay,
            margin + delay_x,
            hdr_y,
            scale(96, d),
            hdr_h,
            true,
        );

        let footer_y = ch - scale(34, d);
        let quit_w = scale(74, d);
        let hide_w = scale(96, d);
        let _ = MoveWindow(self.btn_quit, cw - margin - quit_w, footer_y, quit_w, btn_h, true);
        let _ = MoveWindow(
            self.btn_hide,
            cw - margin - quit_w - scale(8, d) - hide_w,
            footer_y,
            hide_w,
            btn_h,
            true,
        );
        let chk_w = (cw - margin - quit_w - hide_w - scale(24, d)).max(scale(140, d));
        let _ = MoveWindow(self.chk_start_min, margin, footer_y + scale(2, d), chk_w, scale(22, d), true);

        self.layout_rows();
        self.update_scrollbar();
    }

    fn row_columns(&self, panel_w: i32) -> (i32, i32, i32) {
        let d = self.dpi;
        let pad = scale(8, d);
        let delay_col_w = scale(98, d);
        let vol_col_w = scale(150, d);
        let gap = scale(8, d);

        let delay_x = (panel_w - pad - delay_col_w).max(pad + scale(140, d));
        let vol_x = (delay_x - gap - vol_col_w).max(pad + scale(80, d));
        let dev_w = (vol_x - gap - pad).max(scale(60, d));
        (dev_w, vol_x, delay_x)
    }

    unsafe fn layout_rows(&self) {
        let mut panel_rc = RECT::default();
        let _ = GetClientRect(self.scroll_panel, &mut panel_rc);
        let panel_w = (panel_rc.right - panel_rc.left).max(100);
        let (dev_w, vol_x, delay_x) = self.row_columns(panel_w);
        let d = self.dpi;
        let row_h = scale(34, d);
        let pad = scale(8, d);

        for (idx, row) in self.rows.iter().enumerate() {
            let y = scale(6, d) + (idx as i32) * row_h - self.scroll_y;
            let _ = MoveWindow(
                row.checkbox,
                pad,
                y + scale(5, d),
                if row.is_default {
                    (panel_w - pad * 2).max(dev_w)
                } else {
                    dev_w
                },
                scale(22, d),
                true,
            );
            if let Some(tb) = row.trackbar {
                let _ = MoveWindow(tb, vol_x, y + scale(3, d), scale(104, d), scale(26, d), true);
            }
            if let Some(vl) = row.vol_label {
                let _ = MoveWindow(
                    vl,
                    vol_x + scale(106, d),
                    y + scale(5, d),
                    scale(38, d),
                    scale(22, d),
                    true,
                );
            }
            if let Some(ed) = row.delay_edit {
                let _ = MoveWindow(ed, delay_x, y + scale(5, d), scale(68, d), scale(22, d), true);
                if let Some(ud) = row.delay_updown {
                    let _ = SendMessageW(ud, UDM_SETBUDDY, WPARAM(ed.0 as usize), LPARAM(0));
                }
            }
            if let Some(ml) = row.ms_label {
                let _ = MoveWindow(
                    ml,
                    delay_x + scale(72, d),
                    y + scale(5, d),
                    scale(24, d),
                    scale(22, d),
                    true,
                );
            }
        }
    }

    unsafe fn update_scrollbar(&mut self) {
        let mut panel_rc = RECT::default();
        let _ = GetClientRect(self.scroll_panel, &mut panel_rc);
        let view_h = (panel_rc.bottom - panel_rc.top).max(1);
        let max_scroll = (self.scroll_content_h - view_h).max(0);
        if self.scroll_y > max_scroll {
            let delta = self.scroll_y - max_scroll;
            self.scroll_y = max_scroll;
            let _ = ScrollWindowEx(
                self.scroll_panel,
                0,
                delta,
                None,
                None,
                None,
                None,
                SW_SCROLLCHILDREN,
            );
        }

        let si = SCROLLINFO {
            cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
            fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
            nMin: 0,
            nMax: self.scroll_content_h.max(1) - 1,
            nPage: view_h as u32,
            nPos: self.scroll_y,
            nTrackPos: 0,
        };
        let _ = SetScrollInfo(self.scroll_panel, SB_VERT, &si, true);
    }

    unsafe fn scroll_by(&mut self, new_y: i32) {
        let mut panel_rc = RECT::default();
        let _ = GetClientRect(self.scroll_panel, &mut panel_rc);
        let view_h = (panel_rc.bottom - panel_rc.top).max(1);
        let max_scroll = (self.scroll_content_h - view_h).max(0);
        let clamped = new_y.clamp(0, max_scroll);
        if clamped == self.scroll_y {
            return;
        }
        let delta = self.scroll_y - clamped;
        self.scroll_y = clamped;
        let _ = ScrollWindowEx(
            self.scroll_panel,
            0,
            delta,
            None,
            None,
            None,
            None,
            SW_SCROLLCHILDREN,
        );
        let si = SCROLLINFO {
            cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
            fMask: SIF_POS,
            nPos: self.scroll_y,
            ..Default::default()
        };
        let _ = SetScrollInfo(self.scroll_panel, SB_VERT, &si, true);
    }

    unsafe fn destroy_rows(&mut self) {
        for row in self.rows.drain(..) {
            let _ = DestroyWindow(row.checkbox);
            if let Some(h) = row.trackbar {
                let _ = DestroyWindow(h);
            }
            if let Some(h) = row.vol_label {
                let _ = DestroyWindow(h);
            }
            if let Some(h) = row.delay_updown {
                let _ = DestroyWindow(h);
            }
            if let Some(h) = row.delay_edit {
                let _ = DestroyWindow(h);
            }
            if let Some(h) = row.ms_label {
                let _ = DestroyWindow(h);
            }
        }
        self.scroll_y = 0;
        self.scroll_content_h = 0;
    }

    unsafe fn rebuild_rows(&mut self) {
        self.suppress_events = true;
        self.destroy_rows();

        let d = self.dpi;
        let row_h = scale(34, d);

        for (idx, dev) in self.devices.iter().enumerate() {
            let base_id = ROW_BASE_ID + idx * ROW_STRIDE;
            if dev.is_default {
                let label = format!("{}  [Default Source]", dev.name);
                let chk = self.create_child(
                    0,
                    w!("BUTTON"),
                    &label,
                    WS_CHILD.0 | WS_VISIBLE.0 | WS_DISABLED.0 | BS_AUTOCHECKBOX as u32,
                    base_id,
                    self.scroll_panel,
                    false,
                );
                self.rows.push(DeviceRowControls {
                    device_id: dev.id.clone(),
                    is_default: true,
                    checkbox: chk,
                    trackbar: None,
                    vol_label: None,
                    delay_edit: None,
                    delay_updown: None,
                    ms_label: None,
                });
            } else {
                let dev_cfg = self.config.get_device(&dev.id);
                let chk = self.create_child(
                    0,
                    w!("BUTTON"),
                    &dev.name,
                    WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
                    base_id,
                    self.scroll_panel,
                    false,
                );
                let checked = if dev_cfg.enabled {
                    BST_CHECKED
                } else {
                    BST_UNCHECKED
                };
                let _ = SendMessageW(chk, BM_SETCHECK, WPARAM(checked.0 as usize), LPARAM(0));

                let tb = self.create_child(
                    0,
                    TRACKBAR_CLASSW,
                    "",
                    WS_CHILD.0
                        | WS_VISIBLE.0
                        | WS_TABSTOP.0
                        | TBS_HORZ
                        | TBS_BOTH
                        | TBS_NOTICKS,
                    base_id + 1,
                    self.scroll_panel,
                    false,
                );
                let _ = SendMessageW(tb, TBM_SETRANGE, WPARAM(1), LPARAM((100 << 16) as isize));
                let _ = SendMessageW(tb, TBM_SETPOS, WPARAM(1), LPARAM(dev_cfg.volume as isize));

                let vol_lbl = self.create_child(
                    0,
                    w!("STATIC"),
                    &format!("{}%", dev_cfg.volume),
                    WS_CHILD.0 | WS_VISIBLE.0 | SS_RIGHT.0 | SS_CENTERIMAGE.0,
                    base_id + 2,
                    self.scroll_panel,
                    false,
                );

                let ed = self.create_child(
                    0,
                    w!("EDIT"),
                    &dev_cfg.delay_ms.to_string(),
                    WS_CHILD.0
                        | WS_VISIBLE.0
                        | WS_BORDER.0
                        | WS_TABSTOP.0
                        | ES_NUMBER as u32
                        | ES_RIGHT as u32,
                    base_id + 3,
                    self.scroll_panel,
                    false,
                );
                let _ = SendMessageW(ed, EM_SETLIMITTEXT, WPARAM(4), LPARAM(0));

                let ud = self.create_child(
                    0,
                    UPDOWN_CLASSW,
                    "",
                    WS_CHILD.0
                        | WS_VISIBLE.0
                        | UDS_SETBUDDYINT
                        | UDS_ALIGNRIGHT
                        | UDS_ARROWKEYS
                        | UDS_NOTHOUSANDS,
                    base_id + 4,
                    self.scroll_panel,
                    false,
                );
                let _ = SendMessageW(ud, UDM_SETRANGE32, WPARAM(0), LPARAM(MAX_DELAY_MS as isize));
                let _ = SendMessageW(ud, UDM_SETBUDDY, WPARAM(ed.0 as usize), LPARAM(0));
                let _ = SendMessageW(ud, UDM_SETPOS32, WPARAM(0), LPARAM(dev_cfg.delay_ms as isize));

                let ms_lbl = self.create_child(
                    0,
                    w!("STATIC"),
                    "ms",
                    WS_CHILD.0 | WS_VISIBLE.0 | SS_LEFT.0 | SS_CENTERIMAGE.0,
                    base_id + 5,
                    self.scroll_panel,
                    false,
                );

                self.rows.push(DeviceRowControls {
                    device_id: dev.id.clone(),
                    is_default: false,
                    checkbox: chk,
                    trackbar: Some(tb),
                    vol_label: Some(vol_lbl),
                    delay_edit: Some(ed),
                    delay_updown: Some(ud),
                    ms_label: Some(ms_lbl),
                });
            }
        }

        self.scroll_content_h = if self.devices.is_empty() {
            0
        } else {
            scale(12, d) + (self.devices.len() as i32) * row_h
        };

        self.layout_rows();
        self.update_scrollbar();
        self.suppress_events = false;
    }

    unsafe fn refresh_devices_and_sync(&mut self) {
        match enumerate_render_devices() {
            Ok((devices, default_id)) => {
                for dev in &devices {
                    if let Some(entry) = self.config.devices.get_mut(&dev.id) {
                        entry.last_name = dev.name.clone();
                    }
                }
                self.devices = devices;
                self.default_id = default_id;

                let default_label = match &self.default_id {
                    Some(def_id) => {
                        let name = self
                            .devices
                            .iter()
                            .find(|d| &d.id == def_id)
                            .map(|d| d.name.as_str())
                            .unwrap_or("Unknown device");
                        format!("Default source: {name}")
                    }
                    None => "Default source: (No active playback device)".to_string(),
                };
                set_text(self.lbl_default, &default_label);

                self.rebuild_rows();
                let sync_status =
                    self.audio
                        .sync_state(&self.config, &self.devices, self.default_id.as_deref());
                self.update_status_ui(&sync_status);
            }
            Err(err) => {
                self.devices.clear();
                self.default_id = None;
                self.audio.shutdown();
                self.rebuild_rows();
                set_text(self.lbl_default, "Default source: Unavailable");
                self.update_status_ui(&SyncStatus {
                    active_outputs: 0,
                    last_error: Some(err),
                });
            }
        }
    }

    unsafe fn sync_audio_and_status(&mut self) {
        let status = self
            .audio
            .sync_state(&self.config, &self.devices, self.default_id.as_deref());
        self.update_status_ui(&status);
    }

    unsafe fn update_status_ui(&mut self, status: &SyncStatus) {
        set_text(
            self.btn_toggle,
            if self.config.routing_enabled {
                "Pause Routing"
            } else {
                "Start Routing"
            },
        );

        let (status_text, tray_active, tray_tip) = if !self.config.routing_enabled {
            (
                "Paused".to_string(),
                false,
                "AudioSplit - Paused".to_string(),
            )
        } else if self.default_id.is_none() {
            (
                "Waiting for default audio device".to_string(),
                false,
                "AudioSplit - No default device".to_string(),
            )
        } else if let Some(err) = &status.last_error {
            if status.active_outputs > 0 {
                (
                    format!("Routing to {} device(s) (warning: {})", status.active_outputs, err),
                    true,
                    format!("AudioSplit - Active ({} outputs)", status.active_outputs),
                )
            } else {
                (
                    format!("Audio error: {err}"),
                    false,
                    "AudioSplit - Audio error".to_string(),
                )
            }
        } else if status.active_outputs > 0 {
            (
                format!("Routing to {} device(s)", status.active_outputs),
                true,
                format!("AudioSplit - Active ({} outputs)", status.active_outputs),
            )
        } else {
            (
                "Idle - select output(s) below".to_string(),
                false,
                "AudioSplit - Idle".to_string(),
            )
        };

        set_text(self.lbl_status, &status_text);
        self.tray.update(tray_active, &tray_tip);
    }

    unsafe fn schedule_save(&self) {
        let _ = SetTimer(self.hwnd, TIMER_DEBOUNCE_SAVE, 400, None);
    }

    unsafe fn toggle_window_visibility(&self) {
        if IsWindowVisible(self.hwnd).as_bool() {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        } else {
            let _ = ShowWindow(self.hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    unsafe fn toggle_routing(&mut self) {
        self.config.routing_enabled = !self.config.routing_enabled;
        self.sync_audio_and_status();
        let _ = self.config.save();
    }

    unsafe fn handle_command(&mut self, wparam: WPARAM, lparam: LPARAM) {
        if self.suppress_events {
            return;
        }
        let id = (wparam.0 & 0xFFFF) as usize;
        let code = ((wparam.0 >> 16) & 0xFFFF) as u32;
        let ctrl_hwnd = HWND(lparam.0 as *mut c_void);

        match id {
            IDC_BTN_TOGGLE_ROUTING | IDM_TRAY_TOGGLE_ROUTING => {
                self.toggle_routing();
                return;
            }
            IDC_BTN_REFRESH => {
                self.refresh_devices_and_sync();
                return;
            }
            IDC_CHK_START_MINIMIZED => {
                if code == BN_CLICKED {
                    let checked = SendMessageW(self.chk_start_min, BM_GETCHECK, WPARAM(0), LPARAM(0)).0
                        as u32
                        == BST_CHECKED.0;
                    self.config.start_minimized = checked;
                    let _ = self.config.save();
                }
                return;
            }
            IDC_BTN_HIDE => {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
                return;
            }
            IDC_BTN_QUIT | IDM_TRAY_QUIT => {
                let _ = DestroyWindow(self.hwnd);
                return;
            }
            IDM_TRAY_SHOW_HIDE => {
                self.toggle_window_visibility();
                return;
            }
            _ => {}
        }

        if id >= ROW_BASE_ID {
            let row_idx = (id - ROW_BASE_ID) / ROW_STRIDE;
            let field = (id - ROW_BASE_ID) % ROW_STRIDE;
            if row_idx >= self.rows.len() {
                return;
            }
            let device_id = self.rows[row_idx].device_id.clone();
            let dev_name = self
                .devices
                .iter()
                .find(|d| d.id == device_id)
                .map(|d| d.name.clone())
                .unwrap_or_default();

            if field == 0 && code == BN_CLICKED {
                let checked = SendMessageW(ctrl_hwnd, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 as u32
                    == BST_CHECKED.0;
                let entry = self.config.ensure_device_mut(&device_id, &dev_name);
                entry.enabled = checked;
                self.sync_audio_and_status();
                let _ = self.config.save();
            } else if field == 3 && code == EN_CHANGE {
                let text = get_text(ctrl_hwnd);
                let parsed = text.trim().parse::<u32>().unwrap_or(0);
                let clamped = parsed.min(MAX_DELAY_MS);
                if parsed > MAX_DELAY_MS {
                    self.suppress_events = true;
                    set_text(ctrl_hwnd, &clamped.to_string());
                    self.suppress_events = false;
                }
                let entry = self.config.ensure_device_mut(&device_id, &dev_name);
                entry.delay_ms = clamped;
                self.audio.set_device_delay(&device_id, clamped);
                self.schedule_save();
            }
        }
    }

    unsafe fn handle_hscroll(&mut self, lparam: LPARAM) {
        if self.suppress_events {
            return;
        }
        let tb = HWND(lparam.0 as *mut c_void);
        if tb.is_invalid() {
            return;
        }
        let Some(row) = self.rows.iter().find(|r| r.trackbar == Some(tb)) else {
            return;
        };
        let vol = (SendMessageW(tb, TBM_GETPOS, WPARAM(0), LPARAM(0)).0 as u32).min(100);
        if let Some(lbl) = row.vol_label {
            set_text(lbl, &format!("{vol}%"));
        }
        let device_id = row.device_id.clone();
        let dev_name = self
            .devices
            .iter()
            .find(|d| d.id == device_id)
            .map(|d| d.name.clone())
            .unwrap_or_default();
        let entry = self.config.ensure_device_mut(&device_id, &dev_name);
        entry.volume = vol;
        self.audio.set_device_volume(&device_id, vol);
        self.schedule_save();
    }
}

impl Drop for AppState {
    fn drop(&mut self) {
        let _ = self.config.save();
        self.audio.shutdown();
        self.watcher.take();
        self.tray.remove();
        unsafe {
            if !self.font_regular.is_invalid() {
                let _ = DeleteObject(self.font_regular);
            }
            if !self.font_bold.is_invalid() {
                let _ = DeleteObject(self.font_bold);
            }
            if !self.window_icon.is_invalid() {
                let _ = DestroyIcon(self.window_icon);
            }
        }
    }
}

unsafe extern "system" fn scroll_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let parent = GetParent(hwnd).unwrap_or_default();
    match msg {
        WM_COMMAND | WM_HSCROLL => {
            if !parent.is_invalid() {
                return SendMessageW(parent, msg, wparam, lparam);
            }
        }
        WM_CTLCOLORSTATIC => {
            let hdc = HDC(wparam.0 as *mut c_void);
            let child = HWND(lparam.0 as *mut c_void);
            let _ = SetBkMode(hdc, TRANSPARENT);
            let is_disabled = (GetWindowLongW(child, GWL_STYLE) & (WS_DISABLED.0 as i32)) != 0;
            let color = if is_disabled {
                GetSysColor(COLOR_GRAYTEXT)
            } else {
                GetSysColor(COLOR_WINDOWTEXT)
            };
            let _ = SetTextColor(hdc, COLORREF(color));
            let brush = GetSysColorBrush(COLOR_WINDOW);
            return LRESULT(brush.0 as isize);
        }
        WM_VSCROLL => {
            if !parent.is_invalid() {
                let ptr = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut AppState;
                if !ptr.is_null() {
                    let state = &mut *ptr;
                    let code = (wparam.0 & 0xFFFF) as u32;
                    let mut si = SCROLLINFO {
                        cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                        fMask: SIF_ALL,
                        ..Default::default()
                    };
                    let _ = GetScrollInfo(hwnd, SB_VERT, &mut si);
                    let line = scale(34, state.dpi);
                    let page = (si.nPage as i32).max(line);
                    let new_y = match code {
                        x if x == SB_LINEUP.0 as u32 => state.scroll_y - line,
                        x if x == SB_LINEDOWN.0 as u32 => state.scroll_y + line,
                        x if x == SB_PAGEUP.0 as u32 => state.scroll_y - page,
                        x if x == SB_PAGEDOWN.0 as u32 => state.scroll_y + page,
                        x if x == SB_THUMBTRACK.0 as u32 || x == SB_THUMBPOSITION.0 as u32 => {
                            si.nTrackPos
                        }
                        _ => state.scroll_y,
                    };
                    state.scroll_by(new_y);
                    return LRESULT(0);
                }
            }
        }
        WM_MOUSEWHEEL => {
            if !parent.is_invalid() {
                let ptr = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut AppState;
                if !ptr.is_null() {
                    let state = &mut *ptr;
                    let delta = ((wparam.0 >> 16) as u16) as i16;
                    let step = scale(34, state.dpi);
                    let new_y = state.scroll_y - (delta as i32 * step) / 120;
                    state.scroll_by(new_y);
                    return LRESULT(0);
                }
            }
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn main_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CREATE {
        let cs = &*(lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW);
        let hinst = cs.hInstance;
        let mut state = AppState::new(hwnd, hinst);
        state.init_controls();
        let raw_ptr = Box::into_raw(state);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw_ptr as isize);
        return LRESULT(0);
    }

    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut AppState;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let state = &mut *ptr;

    if state.wm_taskbar_created != 0 && msg == state.wm_taskbar_created {
        state.tray.re_add();
        return LRESULT(0);
    }

    match msg {
        WM_SIZE => {
            state.layout_main();
            return LRESULT(0);
        }
        WM_GETMINMAXINFO => {
            let mmi = &mut *(lparam.0 as *mut MINMAXINFO);
            mmi.ptMinTrackSize.x = scale(500, state.dpi);
            mmi.ptMinTrackSize.y = scale(320, state.dpi);
            return LRESULT(0);
        }
        WM_DPICHANGED => {
            let new_dpi = (wparam.0 & 0xFFFF) as u32;
            if new_dpi >= 96 {
                state.dpi = new_dpi;
                let old_reg = state.font_regular;
                let old_bold = state.font_bold;
                state.font_regular = make_font(new_dpi, false);
                state.font_bold = make_font(new_dpi, true);
                for &h in &[
                    state.lbl_default,
                    state.hdr_device,
                    state.hdr_volume,
                    state.hdr_delay,
                ] {
                    set_font(h, state.font_bold);
                }
                for &h in &[
                    state.btn_toggle,
                    state.lbl_status,
                    state.btn_refresh,
                    state.chk_start_min,
                    state.btn_hide,
                    state.btn_quit,
                ] {
                    set_font(h, state.font_regular);
                }
                if !old_reg.is_invalid() {
                    let _ = DeleteObject(old_reg);
                }
                if !old_bold.is_invalid() {
                    let _ = DeleteObject(old_bold);
                }
                let suggested = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                state.rebuild_rows();
                state.layout_main();
            }
            return LRESULT(0);
        }
        WM_CTLCOLORSTATIC => {
            let hdc = HDC(wparam.0 as *mut c_void);
            let _ = SetBkMode(hdc, TRANSPARENT);
            let _ = SetTextColor(hdc, COLORREF(GetSysColor(COLOR_WINDOWTEXT)));
            let brush = GetSysColorBrush(COLOR_BTNFACE);
            return LRESULT(brush.0 as isize);
        }
        WM_COMMAND => {
            state.handle_command(wparam, lparam);
            return LRESULT(0);
        }
        WM_HSCROLL => {
            state.handle_hscroll(lparam);
            return LRESULT(0);
        }
        WM_APP_DEVICES_CHANGED => {
            let _ = SetTimer(hwnd, TIMER_DEBOUNCE_REFRESH, 250, None);
            return LRESULT(0);
        }
        WM_TIMER => {
            match wparam.0 {
                TIMER_DEBOUNCE_REFRESH => {
                    let _ = KillTimer(hwnd, TIMER_DEBOUNCE_REFRESH);
                    state.refresh_devices_and_sync();
                }
                TIMER_DEBOUNCE_SAVE => {
                    let _ = KillTimer(hwnd, TIMER_DEBOUNCE_SAVE);
                    let _ = state.config.save();
                }
                _ => {}
            }
            return LRESULT(0);
        }
        WM_APP_TRAY => {
            let event = (lparam.0 & 0xFFFF) as u32;
            match event {
                WM_LBUTTONUP => {
                    state.toggle_window_visibility();
                }
                WM_RBUTTONUP => {
                    let visible = IsWindowVisible(hwnd).as_bool();
                    state
                        .tray
                        .show_context_menu(visible, state.config.routing_enabled);
                }
                _ => {}
            }
            return LRESULT(0);
        }
        WM_CLOSE => {
            let _ = ShowWindow(hwnd, SW_HIDE);
            return LRESULT(0);
        }
        WM_DESTROY => {
            let _ = KillTimer(hwnd, TIMER_DEBOUNCE_REFRESH);
            let _ = KillTimer(hwnd, TIMER_DEBOUNCE_SAVE);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(ptr));
            PostQuitMessage(0);
            return LRESULT(0);
        }
        _ => {}
    }

    DefWindowProcW(hwnd, msg, wparam, lparam)
}

pub fn run() -> Result<(), String> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None)
            .map_err(|e| format!("GetModuleHandleW failed: {e}"))?
            .into();
        let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();

        let scroll_cls = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(scroll_wnd_proc),
            hInstance: hinst,
            hCursor: cursor,
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as *mut c_void),
            lpszClassName: SCROLL_CLASS,
            ..Default::default()
        };
        if RegisterClassExW(&scroll_cls) == 0 {
            return Err("RegisterClassExW(scroll) failed".to_string());
        }

        let main_cls = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(main_wnd_proc),
            hInstance: hinst,
            hCursor: cursor,
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as *mut c_void),
            lpszClassName: WINDOW_CLASS,
            ..Default::default()
        };
        if RegisterClassExW(&main_cls) == 0 {
            return Err("RegisterClassExW(main) failed".to_string());
        }

        let style = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN;
        let hwnd = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            WINDOW_CLASS,
            w!("AudioSplit"),
            style,
            windows::Win32::UI::WindowsAndMessaging::CW_USEDEFAULT,
            windows::Win32::UI::WindowsAndMessaging::CW_USEDEFAULT,
            600,
            440,
            None,
            None,
            hinst,
            None,
        )
        .map_err(|e| format!("CreateWindowExW failed: {e}"))?;

        let dpi = GetDpiForWindow(hwnd).max(96);
        let mut client_rc = RECT {
            left: 0,
            top: 0,
            right: scale(580, dpi),
            bottom: scale(400, dpi),
        };
        let _ = AdjustWindowRectEx(&mut client_rc, style, false, WS_EX_CONTROLPARENT);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            client_rc.right - client_rc.left,
            client_rc.bottom - client_rc.top,
            windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE | SWP_NOZORDER,
        );

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const AppState;
        let start_minimized = if !ptr.is_null() {
            (*ptr).config.start_minimized
        } else {
            false
        };

        if start_minimized {
            let _ = ShowWindow(hwnd, SW_HIDE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // Let standard dialog tab/arrow navigation work when available, fallback to normal dispatch.
            if !windows::Win32::UI::WindowsAndMessaging::IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    Ok(())
}
