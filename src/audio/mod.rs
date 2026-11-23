pub mod capture;
pub mod format;
pub mod render;
pub mod ringbuf;

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use windows::core::{implement, PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::Media::Audio::{
    eConsole, eRender, EDataFlow, ERole, IAudioClient, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, DEVICE_STATE, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, GetCurrentThread,
    SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::config::AppConfig;
use capture::{CaptureHandle, SinkSlot};
use render::RenderHandle;
use ringbuf::DelayRingBuffer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SyncStatus {
    pub active_outputs: usize,
    pub last_error: Option<String>,
}

pub(super) struct ComGuard {
    should_uninit: bool,
}

impl ComGuard {
    pub fn new_mta() -> Result<Self, String> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() {
            Ok(Self {
                should_uninit: true,
            })
        } else {
            Err(format!("CoInitializeEx(MTA) failed: {hr:?}"))
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

pub(super) struct AudioThreadPriorityGuard {
    avrt_handle: HANDLE,
}

impl AudioThreadPriorityGuard {
    pub fn enter() -> Self {
        let mut task_index = 0u32;
        if let Ok(handle) =
            unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Pro Audio"), &mut task_index) }
        {
            Self {
                avrt_handle: handle,
            }
        } else {
            unsafe {
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
            }
            Self {
                avrt_handle: HANDLE::default(),
            }
        }
    }
}

impl Drop for AudioThreadPriorityGuard {
    fn drop(&mut self) {
        if !self.avrt_handle.is_invalid() && !self.avrt_handle.0.is_null() {
            unsafe {
                let _ = AvRevertMmThreadCharacteristics(self.avrt_handle);
            }
        }
    }
}

pub(super) struct HandleGuard(HANDLE);

impl HandleGuard {
    pub fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    pub fn null() -> Self {
        Self(HANDLE::default())
    }

    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

unsafe fn pwstr_to_string_and_free(pwstr: PWSTR) -> String {
    if pwstr.is_null() {
        return String::new();
    }
    let s = pwstr.to_string().unwrap_or_default();
    CoTaskMemFree(Some(pwstr.0 as *const c_void));
    s
}

pub(super) fn open_device_audio_client(device_id: &str) -> Result<IAudioClient, String> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| format!("CoCreateInstance(MMDeviceEnumerator) failed: {e}"))?;
    let wide: Vec<u16> = device_id.encode_utf16().chain(Some(0)).collect();
    let device = unsafe { enumerator.GetDevice(PCWSTR(wide.as_ptr())) }
        .map_err(|e| format!("GetDevice failed: {e}"))?;
    unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }
        .map_err(|e| format!("Activate(IAudioClient) failed: {e}"))
}

pub fn enumerate_render_devices() -> Result<(Vec<AudioDeviceInfo>, Option<String>), String> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| format!("MMDeviceEnumerator creation failed: {e}"))?;

    let default_id = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
        .ok()
        .and_then(|dev| unsafe { dev.GetId() }.ok())
        .map(|pw| unsafe { pwstr_to_string_and_free(pw) })
        .filter(|s| !s.is_empty());

    let collection = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }
        .map_err(|e| format!("EnumAudioEndpoints failed: {e}"))?;

    let count = unsafe { collection.GetCount() }.map_err(|e| format!("GetCount failed: {e}"))?;
    let mut devices = Vec::with_capacity(count as usize);

    for i in 0..count {
        let Ok(dev) = (unsafe { collection.Item(i) }) else {
            continue;
        };
        let Ok(id_pwstr) = (unsafe { dev.GetId() }) else {
            continue;
        };
        let id = unsafe { pwstr_to_string_and_free(id_pwstr) };
        if id.is_empty() {
            continue;
        }

        let mut name = String::new();
        if let Ok(store) = unsafe { dev.OpenPropertyStore(STGM_READ) } {
            if let Ok(mut prop) = unsafe { store.GetValue(&PKEY_Device_FriendlyName) } {
                let s = prop.to_string();
                if !s.is_empty() {
                    name = s;
                }
                unsafe {
                    let _ = PropVariantClear(&mut prop);
                }
            }
        }
        if name.is_empty() {
            name = format!("Audio Output {}", i + 1);
        }

        let is_default = default_id.as_deref() == Some(id.as_str());
        devices.push(AudioDeviceInfo {
            id,
            name,
            is_default,
        });
    }

    devices.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok((devices, default_id))
}

#[implement(IMMNotificationClient)]
struct EndpointNotifier {
    hwnd_raw: isize,
    msg_id: u32,
}

impl EndpointNotifier {
    fn notify(&self) {
        if self.hwnd_raw != 0 {
            unsafe {
                let _ = PostMessageW(
                    HWND(self.hwnd_raw as *mut c_void),
                    self.msg_id,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
}

impl IMMNotificationClient_Impl for EndpointNotifier_Impl {
    fn OnDeviceStateChanged(
        &self,
        _pwstrdeviceid: &PCWSTR,
        _dwnewstate: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        self.notify();
        Ok(())
    }

    fn OnDeviceAdded(&self, _pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
        self.notify();
        Ok(())
    }

    fn OnDeviceRemoved(&self, _pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
        self.notify();
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        _pwstrdefaultdeviceid: &PCWSTR,
    ) -> windows::core::Result<()> {
        if flow == eRender && role == eConsole {
            self.notify();
        }
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _pwstrdeviceid: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        // Intentionally ignored so volume/mute adjustments don't trigger device list rebuilds.
        Ok(())
    }
}

pub struct DeviceNotificationWatcher {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
}

impl DeviceNotificationWatcher {
    pub fn register(hwnd: HWND, msg_id: u32) -> Result<Self, String> {
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|e| format!("CoCreateInstance(IMMDeviceEnumerator) failed: {e}"))?;
        let client: IMMNotificationClient = EndpointNotifier {
            hwnd_raw: hwnd.0 as isize,
            msg_id,
        }
        .into();

        unsafe { enumerator.RegisterEndpointNotificationCallback(&client) }
            .map_err(|e| format!("RegisterEndpointNotificationCallback failed: {e}"))?;

        Ok(Self { enumerator, client })
    }
}

impl Drop for DeviceNotificationWatcher {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .enumerator
                .UnregisterEndpointNotificationCallback(&self.client);
        }
    }
}

struct ActiveRender {
    _handle: RenderHandle,
    volume: Arc<AtomicU32>,
    delay_ms: Arc<AtomicU32>,
}

pub struct AudioManager {
    capture: Option<CaptureHandle>,
    sinks: Arc<RwLock<Vec<SinkSlot>>>,
    renders: HashMap<String, ActiveRender>,
    on_lost: Arc<dyn Fn() + Send + Sync>,
}

impl AudioManager {
    pub fn new(on_lost: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            capture: None,
            sinks: Arc::new(RwLock::new(Vec::new())),
            renders: HashMap::new(),
            on_lost,
        }
    }

    pub fn sync_state(
        &mut self,
        config: &AppConfig,
        devices: &[AudioDeviceInfo],
        default_id: Option<&str>,
    ) -> SyncStatus {
        let mut status = SyncStatus::default();

        let desired_ids = select_target_devices(config, devices, default_id);

        let Some(def_id) = default_id else {
            self.shutdown();
            return status;
        };

        if !config.routing_enabled || desired_ids.is_empty() {
            self.shutdown();
            return status;
        }

        // Restart capture (and dependent renders so sample rate & buffers match) if default changed.
        let capture_matches = self
            .capture
            .as_ref()
            .map(|c| c.device_id == def_id)
            .unwrap_or(false);

        if !capture_matches {
            self.shutdown();
            match CaptureHandle::start(
                def_id.to_string(),
                Arc::clone(&self.sinks),
                Arc::clone(&self.on_lost),
            ) {
                Ok(cap) => {
                    self.capture = Some(cap);
                }
                Err(err) => {
                    status.last_error = Some(err);
                    return status;
                }
            }
        }

        let capture_sr = match &self.capture {
            Some(c) => c.sample_rate,
            None => return status,
        };

        let desired_set: HashSet<&str> = desired_ids.iter().map(|d| d.id.as_str()).collect();

        // Stop renders that are no longer desired (disabled, removed, or became the default device).
        let to_remove: Vec<String> = self
            .renders
            .keys()
            .filter(|id| !desired_set.contains(id.as_str()))
            .cloned()
            .collect();

        if !to_remove.is_empty() {
            if let Ok(mut guard) = self.sinks.write() {
                guard.retain(|slot| !to_remove.contains(&slot.device_id));
            }
            for id in to_remove {
                self.renders.remove(&id);
            }
        }

        // Update existing renders or spawn newly enabled ones.
        for dev in desired_ids {
            let dev_cfg = config.get_device(&dev.id);
            if let Some(existing) = self.renders.get(&dev.id) {
                existing.volume.store(dev_cfg.volume, Ordering::Relaxed);
                existing.delay_ms.store(dev_cfg.delay_ms, Ordering::Relaxed);
                continue;
            }

            let ring = Arc::new(Mutex::new(DelayRingBuffer::new(capture_sr)));
            let vol = Arc::new(AtomicU32::new(dev_cfg.volume));
            let delay = Arc::new(AtomicU32::new(dev_cfg.delay_ms));

            if let Ok(mut guard) = self.sinks.write() {
                guard.push(SinkSlot {
                    device_id: dev.id.clone(),
                    ring: Arc::clone(&ring),
                });
            }

            match RenderHandle::start(
                dev.id.clone(),
                capture_sr,
                ring,
                Arc::clone(&vol),
                Arc::clone(&delay),
                Arc::clone(&self.on_lost),
            ) {
                Ok(handle) => {
                    self.renders.insert(
                        dev.id.clone(),
                        ActiveRender {
                            _handle: handle,
                            volume: vol,
                            delay_ms: delay,
                        },
                    );
                }
                Err(err) => {
                    if let Ok(mut guard) = self.sinks.write() {
                        guard.retain(|s| s.device_id != dev.id);
                    }
                    if status.last_error.is_none() {
                        status.last_error = Some(format!("{}: {}", dev.name, err));
                    }
                }
            }
        }

        status.active_outputs = self.renders.len();
        if self.renders.is_empty() {
            if let Some(mut cap) = self.capture.take() {
                cap.stop();
            }
        }

        status
    }

    pub fn set_device_volume(&self, id: &str, volume: u32) {
        if let Some(render) = self.renders.get(id) {
            render.volume.store(volume.min(100), Ordering::Relaxed);
        }
    }

    pub fn set_device_delay(&self, id: &str, delay_ms: u32) {
        if let Some(render) = self.renders.get(id) {
            render.delay_ms.store(delay_ms.min(1000), Ordering::Relaxed);
        }
    }

    pub fn shutdown(&mut self) {
        if let Ok(mut guard) = self.sinks.write() {
            guard.clear();
        }
        self.renders.clear();
        if let Some(mut cap) = self.capture.take() {
            cap.stop();
        }
    }
}

impl Drop for AudioManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Strictly excludes the current default capture device from render targets to prevent feedback loops.
pub(crate) fn select_target_devices<'a>(
    config: &AppConfig,
    devices: &'a [AudioDeviceInfo],
    default_id: Option<&str>,
) -> Vec<&'a AudioDeviceInfo> {
    devices
        .iter()
        .filter(|d| {
            Some(d.id.as_str()) != default_id
                && config.devices.get(&d.id).map(|c| c.enabled).unwrap_or(false)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeviceConfig;

    #[test]
    fn excludes_default_device_even_if_enabled_in_config() {
        let mut cfg = AppConfig::default();
        cfg.devices.insert(
            "dev-default".to_string(),
            DeviceConfig {
                enabled: true,
                ..Default::default()
            },
        );
        cfg.devices.insert(
            "dev-headphones".to_string(),
            DeviceConfig {
                enabled: true,
                ..Default::default()
            },
        );
        cfg.devices.insert(
            "dev-hdmi".to_string(),
            DeviceConfig {
                enabled: false,
                ..Default::default()
            },
        );

        let devices = vec![
            AudioDeviceInfo {
                id: "dev-default".to_string(),
                name: "Speakers".to_string(),
                is_default: true,
            },
            AudioDeviceInfo {
                id: "dev-headphones".to_string(),
                name: "Headphones".to_string(),
                is_default: false,
            },
            AudioDeviceInfo {
                id: "dev-hdmi".to_string(),
                name: "HDMI".to_string(),
                is_default: false,
            },
        ];

        let targets = select_target_devices(&cfg, &devices, Some("dev-default"));
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id, "dev-headphones");

        // When default swaps to Headphones, Speakers becomes eligible and Headphones is excluded.
        let targets_swapped = select_target_devices(&cfg, &devices, Some("dev-headphones"));
        assert_eq!(targets_swapped.len(), 1);
        assert_eq!(targets_swapped[0].id, "dev-default");
    }
}
