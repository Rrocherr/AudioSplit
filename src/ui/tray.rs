use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos,
    PostMessageW, SetForegroundWindow, SetMenuDefaultItem, TrackPopupMenu, HICON, ICONINFO,
    MF_CHECKED, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
    TPM_RIGHTBUTTON, WM_NULL,
};

pub const IDM_TRAY_SHOW_HIDE: usize = 3001;
pub const IDM_TRAY_TOGGLE_ROUTING: usize = 3002;
pub const IDM_TRAY_QUIT: usize = 3003;

fn blend_pixel(bg: u32, fg: u32, alpha: f32) -> u32 {
    let a = alpha.clamp(0.0, 1.0);
    let bg_a = ((bg >> 24) & 0xFF) as f32;
    let bg_r = ((bg >> 16) & 0xFF) as f32;
    let bg_g = ((bg >> 8) & 0xFF) as f32;
    let bg_b = (bg & 0xFF) as f32;

    let fg_r = ((fg >> 16) & 0xFF) as f32;
    let fg_g = ((fg >> 8) & 0xFF) as f32;
    let fg_b = (fg & 0xFF) as f32;

    let out_a = (bg_a.max(a * 255.0)).round().clamp(0.0, 255.0) as u32;
    let out_r = (bg_r * (1.0 - a) + fg_r * a).round().clamp(0.0, 255.0) as u32;
    let out_g = (bg_g * (1.0 - a) + fg_g * a).round().clamp(0.0, 255.0) as u32;
    let out_b = (bg_b * (1.0 - a) + fg_b * a).round().clamp(0.0, 255.0) as u32;

    (out_a << 24) | (out_r << 16) | (out_g << 8) | out_b
}

/// Renders a crisp 32x32 ARGB speaker + wave icon programmatically without .ico resources.
pub fn create_app_icon(active: bool) -> HICON {
    const W: i32 = 32;
    const H: i32 = 32;

    let mut pixels = [0u32; (W * H) as usize];
    let bg_color = if active { 0xFF1E3A5F } else { 0xFF2D3748 };
    let wave_color = if active { 0xFF34D399 } else { 0xFF94A3B8 };
    let dot_color = if active { 0xFF22C55E } else { 0xFF94A3B8 };
    let white = 0xFFF8FAFCu32;

    for y in 0..H {
        for x in 0..W {
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;
            let mut px = 0u32;

            // Rounded badge background (4 px corner radius, 1 px margin)
            let dx = (fx - 16.0).abs() - 10.5;
            let dy = (fy - 16.0).abs() - 10.5;
            let sdf = dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - 4.5;
            if sdf < 0.7 {
                let edge = (0.7 - sdf).clamp(0.0, 1.0);
                let alpha = (edge * 255.0).round() as u32;
                px = (alpha << 24) | (bg_color & 0x00FF_FFFF);
            }

            if px != 0 {
                // Speaker body box: x in [7..11.5], y in [12.5..19.5]
                if (7.0..=11.5).contains(&fx) && (12.5..=19.5).contains(&fy) {
                    px = blend_pixel(px, white, 1.0);
                }
                // Speaker flare cone: x in [11.5..16.5]
                if (11.5..=16.5).contains(&fx) {
                    let t = (fx - 11.5) / 5.0;
                    let half_h = 3.5 + t * 4.5;
                    if (fy - 16.0).abs() <= half_h {
                        px = blend_pixel(px, white, 1.0);
                    }
                }

                // Dual sound wave arcs centered at (15.5, 16.0) for x >= 17.5
                if fx >= 17.5 && (fy - 16.0).abs() <= (fx - 14.5) * 1.15 {
                    let dist = (fx - 15.5).hypot(fy - 16.0);
                    if (dist - 4.8).abs() < 0.95 {
                        let cov = (1.0 - ((dist - 4.8).abs() / 0.95)).clamp(0.0, 1.0);
                        px = blend_pixel(px, wave_color, cov);
                    }
                    if (dist - 8.2).abs() < 0.95 {
                        let cov = (1.0 - ((dist - 8.2).abs() / 0.95)).clamp(0.0, 1.0);
                        px = blend_pixel(px, wave_color, cov);
                    }
                }

                // Small status dot in bottom-right corner (24.5, 24.5)
                let dot_dist = (fx - 24.5).hypot(fy - 24.5);
                if dot_dist <= 3.3 {
                    let cov = (3.3 - dot_dist).clamp(0.0, 1.0);
                    px = blend_pixel(px, dot_color, cov);
                }
            }

            pixels[(y * W + x) as usize] = px;
        }
    }

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: W,
            biHeight: -H, // top-down DIB
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [Default::default()],
    };

    unsafe {
        let mut bits_ptr: *mut c_void = std::ptr::null_mut();
        let Ok(hbm_color) =
            CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0)
        else {
            return HICON::default();
        };

        if !bits_ptr.is_null() {
            std::ptr::copy_nonoverlapping(
                pixels.as_ptr(),
                bits_ptr as *mut u32,
                pixels.len(),
            );
        }

        let hbm_mask: HBITMAP = CreateBitmap(W, H, 1, 1, None);
        let icon_info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };

        let hicon = CreateIconIndirect(&icon_info).unwrap_or_default();
        let _ = DeleteObject(hbm_color);
        if !hbm_mask.is_invalid() {
            let _ = DeleteObject(hbm_mask);
        }
        hicon
    }
}

pub struct TrayIcon {
    hwnd: HWND,
    callback_msg: u32,
    active_icon: HICON,
    idle_icon: HICON,
    added: bool,
    current_active: bool,
    current_tip: String,
}

impl TrayIcon {
    pub fn new(hwnd: HWND, callback_msg: u32) -> Self {
        Self {
            hwnd,
            callback_msg,
            active_icon: create_app_icon(true),
            idle_icon: create_app_icon(false),
            added: false,
            current_active: false,
            current_tip: "AudioSplit".to_string(),
        }
    }

    pub fn add(&mut self, active: bool, tooltip: &str) {
        self.current_active = active;
        self.current_tip = tooltip.to_string();
        let nid = self.build_nid(active, tooltip);
        let ok = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool();
        if ok {
            self.added = true;
        }
    }

    pub fn update(&mut self, active: bool, tooltip: &str) {
        self.current_active = active;
        self.current_tip = tooltip.to_string();
        let nid = self.build_nid(active, tooltip);
        if self.added {
            if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) }.as_bool() {
                self.added = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool();
            }
        } else {
            self.added = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool();
        }
    }

    pub fn re_add(&mut self) {
        self.added = false;
        let active = self.current_active;
        let tip = self.current_tip.clone();
        self.add(active, &tip);
    }

    pub fn remove(&mut self) {
        if self.added {
            let nid = self.build_nid(self.current_active, &self.current_tip);
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            }
            self.added = false;
        }
    }

    fn build_nid(&self, active: bool, tooltip: &str) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: self.callback_msg,
            hIcon: if active {
                self.active_icon
            } else {
                self.idle_icon
            },
            ..Default::default()
        };

        let mut i = 0usize;
        for ch in tooltip.encode_utf16() {
            if i + 1 >= nid.szTip.len() {
                break;
            }
            nid.szTip[i] = ch;
            i += 1;
        }
        nid.szTip[i] = 0;
        nid
    }

    pub fn show_context_menu(&self, is_visible: bool, routing_enabled: bool) {
        unsafe {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(self.hwnd);

            let Ok(menu) = CreatePopupMenu() else {
                return;
            };

            let show_label: Vec<u16> = if is_visible {
                "Hide Window\0"
            } else {
                "Show Window\0"
            }
            .encode_utf16()
            .collect();
            let route_label: Vec<u16> = "Routing Active\0".encode_utf16().collect();
            let quit_label: Vec<u16> = "Quit\0".encode_utf16().collect();

            let _ = AppendMenuW(
                menu,
                MF_STRING,
                IDM_TRAY_SHOW_HIDE,
                PCWSTR(show_label.as_ptr()),
            );
            let _ = AppendMenuW(
                menu,
                MF_STRING | if routing_enabled { MF_CHECKED } else { MF_UNCHECKED },
                IDM_TRAY_TOGGLE_ROUTING,
                PCWSTR(route_label.as_ptr()),
            );
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, IDM_TRAY_QUIT, PCWSTR(quit_label.as_ptr()));
            let _ = SetMenuDefaultItem(menu, IDM_TRAY_SHOW_HIDE as u32, 0);

            let _ = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN,
                pt.x,
                pt.y,
                0,
                self.hwnd,
                None,
            );
            let _ = PostMessageW(self.hwnd, WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
        }
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        self.remove();
        unsafe {
            if !self.active_icon.is_invalid() {
                let _ = DestroyIcon(self.active_icon);
            }
            if !self.idle_icon.is_invalid() {
                let _ = DestroyIcon(self.idle_icon);
            }
        }
    }
}
