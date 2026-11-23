use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    IAudioRenderClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects};

use super::format::{
    encode_from_stereo_f32, make_stereo_f32_waveformat, parse_waveformat, AudioFormatInfo,
};
use super::ringbuf::DelayRingBuffer;
use super::{open_device_audio_client, AudioThreadPriorityGuard, ComGuard, HandleGuard};

pub struct RenderHandle {
    stop_flag: Arc<AtomicBool>,
    stop_event_raw: isize,
    thread: Option<JoinHandle<()>>,
}

impl RenderHandle {
    pub fn start(
        device_id: String,
        capture_sample_rate: u32,
        ring: Arc<Mutex<DelayRingBuffer>>,
        volume: Arc<AtomicU32>,
        delay_ms: Arc<AtomicU32>,
        on_lost: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, String> {
        let stop_event = unsafe { CreateEventW(None, true, false, None) }
            .map_err(|e| format!("CreateEventW failed: {e}"))?;
        let stop_event_raw = stop_event.0 as isize;
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_thread = Arc::clone(&stop_flag);
        let dev_id_thread = device_id.clone();

        let (tx, rx) = mpsc::sync_channel::<Result<(), String>>(1);

        let thread = thread::Builder::new()
            .name("audiosplit-render".to_string())
            .spawn(move || {
                render_thread(
                    dev_id_thread,
                    capture_sample_rate,
                    ring,
                    volume,
                    delay_ms,
                    stop_flag_thread,
                    stop_event_raw,
                    tx,
                    on_lost,
                );
            })
            .map_err(|e| {
                unsafe {
                    let _ = CloseHandle(HANDLE(stop_event_raw as *mut _));
                }
                format!("Failed to spawn render thread: {e}")
            })?;

        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                stop_flag,
                stop_event_raw,
                thread: Some(thread),
            }),
            Ok(Err(err)) => {
                stop_flag.store(true, Ordering::Release);
                unsafe {
                    let _ = SetEvent(HANDLE(stop_event_raw as *mut _));
                }
                let _ = thread.join();
                unsafe {
                    let _ = CloseHandle(HANDLE(stop_event_raw as *mut _));
                }
                Err(err)
            }
            Err(_) => {
                stop_flag.store(true, Ordering::Release);
                unsafe {
                    let _ = SetEvent(HANDLE(stop_event_raw as *mut _));
                }
                let _ = thread.join();
                unsafe {
                    let _ = CloseHandle(HANDLE(stop_event_raw as *mut _));
                }
                Err("Timed out initializing audio output".to_string())
            }
        }
    }

    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        if self.stop_event_raw != 0 {
            unsafe {
                let _ = SetEvent(HANDLE(self.stop_event_raw as *mut _));
            }
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        if self.stop_event_raw != 0 {
            unsafe {
                let _ = CloseHandle(HANDLE(self.stop_event_raw as *mut _));
            }
            self.stop_event_raw = 0;
        }
    }
}

impl Drop for RenderHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

enum RenderPath {
    DirectStereoF32,
    FallbackFormat {
        fmt: AudioFormatInfo,
        rate_ratio: f64,
    },
}

fn render_thread(
    device_id: String,
    capture_sample_rate: u32,
    ring: Arc<Mutex<DelayRingBuffer>>,
    volume: Arc<AtomicU32>,
    delay_ms: Arc<AtomicU32>,
    stop_flag: Arc<AtomicBool>,
    stop_event_raw: isize,
    tx: mpsc::SyncSender<Result<(), String>>,
    on_lost: Arc<dyn Fn() + Send + Sync>,
) {
    let _com = match ComGuard::new_mta() {
        Ok(g) => g,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };
    let _prio = AudioThreadPriorityGuard::enter();

    let audio_client = match open_device_audio_client(&device_id) {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };

    let desired_wfx = make_stereo_f32_waveformat(capture_sample_rate);
    let primary_flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

    let primary_ok = unsafe {
        audio_client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            primary_flags,
            300_000,
            0,
            &desired_wfx.Format,
            None,
        )
    }
    .is_ok();

    let render_path = if primary_ok {
        RenderPath::DirectStereoF32
    } else {
        let mix_fmt_ptr = match unsafe { audio_client.GetMixFormat() } {
            Ok(p) => p,
            Err(e) => {
                let _ = tx.send(Err(format!("Render GetMixFormat failed: {e}")));
                return;
            }
        };
        let parsed = unsafe { parse_waveformat(mix_fmt_ptr) };
        let init_fb = parsed.and_then(|fmt| {
            unsafe {
                audio_client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    300_000,
                    0,
                    mix_fmt_ptr,
                    None,
                )
            }
            .map_err(|e| format!("Render Initialize failed: {e}"))?;
            Ok(fmt)
        });
        unsafe {
            CoTaskMemFree(Some(mix_fmt_ptr as *const _));
        }
        match init_fb {
            Ok(fmt) => {
                let rate_ratio = (capture_sample_rate as f64) / (fmt.sample_rate as f64);
                RenderPath::FallbackFormat { fmt, rate_ratio }
            }
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        }
    };

    let buffer_event = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(h) => HandleGuard::new(h),
        Err(e) => {
            let _ = tx.send(Err(format!("CreateEventW(buffer) failed: {e}")));
            return;
        }
    };

    if let Err(e) = unsafe { audio_client.SetEventHandle(buffer_event.raw()) } {
        let _ = tx.send(Err(format!("SetEventHandle failed: {e}")));
        return;
    }

    let buffer_frames = match unsafe { audio_client.GetBufferSize() } {
        Ok(n) if n > 0 => n,
        Ok(_) => {
            let _ = tx.send(Err("Render buffer size is 0".to_string()));
            return;
        }
        Err(e) => {
            let _ = tx.send(Err(format!("GetBufferSize failed: {e}")));
            return;
        }
    };

    let render_client: IAudioRenderClient = match unsafe { audio_client.GetService() } {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Err(format!("GetService(IAudioRenderClient) failed: {e}")));
            return;
        }
    };

    // Prefill with silence before Start() per WASAPI specification.
    if let Ok(ptr) = unsafe { render_client.GetBuffer(buffer_frames) } {
        let _ = ptr;
        let _ = unsafe {
            render_client.ReleaseBuffer(buffer_frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
        };
    }

    if let Err(e) = unsafe { audio_client.Start() } {
        let _ = tx.send(Err(format!("Render Start failed: {e}")));
        return;
    }

    let _ = tx.send(Ok(()));

    let stop_event = HANDLE(stop_event_raw as *mut _);
    let handles = [stop_event, buffer_event.raw()];
    let mut scratch_stereo: Vec<f32> = vec![0.0; (buffer_frames as usize) * 2];
    let mut stream_lost = false;

    while !stop_flag.load(Ordering::Acquire) {
        let wait_res = unsafe { WaitForMultipleObjects(&handles, false, 50) };
        if wait_res == WAIT_OBJECT_0 || stop_flag.load(Ordering::Acquire) {
            break;
        }

        let padding = match unsafe { audio_client.GetCurrentPadding() } {
            Ok(p) => p,
            Err(_) => {
                stream_lost = true;
                break;
            }
        };

        let frames_to_write = buffer_frames.saturating_sub(padding);
        if frames_to_write == 0 {
            continue;
        }

        let out_ptr = match unsafe { render_client.GetBuffer(frames_to_write) } {
            Ok(p) => p,
            Err(_) => {
                stream_lost = true;
                break;
            }
        };

        let cur_vol = volume.load(Ordering::Relaxed);
        let cur_delay = delay_ms.load(Ordering::Relaxed);

        match &render_path {
            RenderPath::DirectStereoF32 => {
                let sample_len = (frames_to_write as usize) * 2;
                let out_slice =
                    unsafe { std::slice::from_raw_parts_mut(out_ptr as *mut f32, sample_len) };
                if let Ok(mut guard) = ring.lock() {
                    guard.read_frames(out_slice, cur_delay, cur_vol, 1.0);
                } else {
                    out_slice.fill(0.0);
                }
            }
            RenderPath::FallbackFormat { fmt, rate_ratio } => {
                let needed = (frames_to_write as usize) * 2;
                if scratch_stereo.len() < needed {
                    scratch_stereo.resize(needed, 0.0);
                }
                let src_slice = &mut scratch_stereo[..needed];
                if let Ok(mut guard) = ring.lock() {
                    guard.read_frames(src_slice, cur_delay, cur_vol, *rate_ratio);
                } else {
                    src_slice.fill(0.0);
                }
                let byte_len = (frames_to_write as usize) * (fmt.block_align as usize);
                let out_bytes = unsafe { std::slice::from_raw_parts_mut(out_ptr, byte_len) };
                encode_from_stereo_f32(src_slice, fmt, out_bytes);
            }
        }

        if unsafe { render_client.ReleaseBuffer(frames_to_write, 0) }.is_err() {
            stream_lost = true;
            break;
        }
    }

    unsafe {
        let _ = audio_client.Stop();
    }

    if stream_lost && !stop_flag.load(Ordering::Acquire) {
        on_lost();
    }
}
