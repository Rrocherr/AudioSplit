#![cfg_attr(not(test), windows_subsystem = "windows")]

mod audio;
mod config;
mod ui;

use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::ApplicationInstallationAndServicing::{
    ActivateActCtx, CreateActCtxW, ACTCTXW,
};
use windows::Win32::System::WindowsProgramming::{
    ACTCTX_FLAG_ASSEMBLY_DIRECTORY_VALID, ACTCTX_FLAG_RESOURCE_NAME_VALID,
    ACTCTX_FLAG_SET_PROCESS_DEFAULT,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::{
    ICC_BAR_CLASSES, ICC_STANDARD_CLASSES, ICC_UPDOWN_CLASS, INITCOMMONCONTROLSEX,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

struct SingleInstanceGuard(HANDLE);

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Activates ComCtl32 v6 visual styles via shell32.dll's embedded manifest (ID 124)
/// and loads comctl32 dynamically afterward so no .rc/windres step is required.
unsafe fn enable_visual_styles() {
    let mut sys_dir = [0u16; 260];
    let len = GetSystemDirectoryW(Some(&mut sys_dir));
    if len == 0 || (len as usize) >= sys_dir.len() {
        return;
    }

    let suffix: Vec<u16> = "\\shell32.dll\0".encode_utf16().collect();
    let mut dll_path = Vec::with_capacity(len as usize + suffix.len());
    dll_path.extend_from_slice(&sys_dir[..len as usize]);
    dll_path.extend_from_slice(&suffix);

    let mut sys_dir_z = Vec::with_capacity(len as usize + 1);
    sys_dir_z.extend_from_slice(&sys_dir[..len as usize]);
    sys_dir_z.push(0);

    let act_ctx = ACTCTXW {
        cbSize: std::mem::size_of::<ACTCTXW>() as u32,
        dwFlags: ACTCTX_FLAG_RESOURCE_NAME_VALID
            | ACTCTX_FLAG_SET_PROCESS_DEFAULT
            | ACTCTX_FLAG_ASSEMBLY_DIRECTORY_VALID,
        lpSource: PCWSTR(dll_path.as_ptr()),
        lpAssemblyDirectory: PCWSTR(sys_dir_z.as_ptr()),
        lpResourceName: PCWSTR(124usize as *const u16),
        ..Default::default()
    };

    if let Ok(h_act) = CreateActCtxW(&act_ctx) {
        if !h_act.is_invalid() {
            let mut cookie = 0usize;
            let _ = ActivateActCtx(h_act, &mut cookie);
        }
    }

    if let Ok(hmod) = LoadLibraryW(w!("comctl32.dll")) {
        if let Some(proc_addr) = GetProcAddress(hmod, s!("InitCommonControlsEx")) {
            type InitFn = unsafe extern "system" fn(*const INITCOMMONCONTROLSEX) -> i32;
            let init_fn: InitFn = std::mem::transmute(proc_addr);
            let icex = INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_STANDARD_CLASSES | ICC_BAR_CLASSES | ICC_UPDOWN_CLASS,
            };
            let _ = init_fn(&icex);
        }
    }
}

fn run_check_mode(out_path: Option<&str>) -> i32 {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() {
            return 1;
        }
    }

    let result = audio::enumerate_render_devices();
    unsafe {
        CoUninitialize();
    }

    match result {
        Ok((devices, default_id)) => {
            let mut report = String::new();
            report.push_str(&format!(
                "default_id: {}\n",
                default_id.as_deref().unwrap_or("<none>")
            ));
            report.push_str(&format!("device_count: {}\n", devices.len()));
            for dev in &devices {
                let marker = if dev.is_default { "*" } else { " " };
                report.push_str(&format!("[{}] {} | {}\n", marker, dev.name, dev.id));
            }
            if let Some(path) = out_path {
                if std::fs::write(path, &report).is_err() {
                    return 2;
                }
            }
            0
        }
        Err(err) => {
            if let Some(path) = out_path {
                let _ = std::fs::write(path, format!("error: {err}\n"));
            }
            1
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("--check") {
        let code = run_check_mode(args.get(2).map(|s| s.as_str()));
        std::process::exit(code);
    }

    let _instance_guard = unsafe {
        match CreateMutexW(None, false, w!("Local\\AudioSplitSingleInstanceMutex")) {
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    if let Ok(existing) = FindWindowW(ui::WINDOW_CLASS, None) {
                        if !existing.is_invalid() {
                            let _ = ShowWindow(existing, SW_RESTORE);
                            let _ = SetForegroundWindow(existing);
                        }
                    }
                    let _ = CloseHandle(h);
                    return;
                }
                SingleInstanceGuard(h)
            }
            Err(_) => SingleInstanceGuard(HANDLE::default()),
        }
    };

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        enable_visual_styles();
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if hr.is_ok() {
            let _ = ui::run();
            CoUninitialize();
        }
    }
}
