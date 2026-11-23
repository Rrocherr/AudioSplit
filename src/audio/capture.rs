use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    CreateEventW, CreateWaitableTimerExW, SetEvent, SetWaitableTimer, WaitForMultipleObjects,
    WaitForSingleObject, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
};

use super::format::{decode_to_stereo_f32, parse_waveformat};
use super::ringbuf::DelayRingBuffer;
use super::{open_device_audio_client, AudioThreadPriorityGuard, ComGuard, HandleGuard};

pub struct SinkSlot {
    pub device_id: String,
    pub ring: Arc<Mutex<DelayRingBuffer>>,
}

pub struct CaptureHandle {
    stop_flag: Arc<AtomicBool>,
    stop_event_raw: isize,
    thread: Option<JoinHandle<()>>,
    pub device_id: String,
    pub sample_rate: u32,
}

impl CaptureHandle {
    pub fn start(
        device_id: String,
        sinks: Arc<RwLock<Vec<SinkSlot>>>,
        on_lost: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, String> {
        let stop_event = unsafe { CreateEventW(None, true, false, None) }
            .map_err(|e| format!("CreateEventW failed: {e}"))?;
        let stop_event_raw = stop_event.0 as isize;
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_thread = Arc::clone(&stop_flag);
        let dev_id_thread = device_id.clone();

        let (tx, rx) = mpsc::sync_channel::<Result<u32, String>>(1);

        let thread = thread::Builder::new()
            .name("audiosplit-capture".to_string())
            .spawn(move || {
                capture_thread(
                    dev_id_thread,
                    sinks,
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
                format!("Failed to spawn capture thread: {e}")
            })?;

        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(sample_rate)) => Ok(Self {
                stop_flag,
                stop_event_raw,
                thread: Some(thread),
                device_id,
                sample_rate,
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
                Err("Timed out initializing loopback capture".to_string())
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

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn broadcast_to_sinks(sinks: &RwLock<Vec<SinkSlot>>, stereo: &[f32]) {
    if stereo.is_empty() {
        return;
    }
    if let Ok(guard) = sinks.read() {
        for slot in guard.iter() {
            if let Ok(mut ring) = slot.ring.lock() {
                ring.push_frames(stereo);
            }
        }
    }
}

fn query_qpc() -> (i64, i64) {
    let mut counter = 0i64;
    let mut freq = 1i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut counter);
        let _ = QueryPerformanceFrequency(&mut freq);
    }
    (counter, freq.max(1))
}

fn capture_thread(
    device_id: String,
    sinks: Arc<RwLock<Vec<SinkSlot>>>,
    stop_flag: Arc<AtomicBool>,
    stop_event_raw: isize,
    tx: mpsc::SyncSender<Result<u32, String>>,
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

    let mix_fmt_ptr = match unsafe { audio_client.GetMixFormat() } {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(Err(format!("GetMixFormat failed: {e}")));
            return;
        }
    };

    let fmt_res = unsafe { parse_waveformat(mix_fmt_ptr) };
    let init_res = fmt_res.and_then(|fmt| {
        // Loopback capture MUST NOT use AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM.
        unsafe {
            audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                300_000, // 30 ms in 100-ns units
                0,
                mix_fmt_ptr,
                None,
            )
        }
        .map_err(|e| format!("Loopback Initialize failed: {e}"))?;
        Ok(fmt)
    });

    unsafe {
        CoTaskMemFree(Some(mix_fmt_ptr as *const _));
    }

    let fmt = match init_res {
        Ok(f) => f,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };

    let capture_client: IAudioCaptureClient = match unsafe { audio_client.GetService() } {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Err(format!("GetService(IAudioCaptureClient) failed: {e}")));
            return;
        }
    };

    if let Err(e) = unsafe { audio_client.Start() } {
        let _ = tx.send(Err(format!("Capture Start failed: {e}")));
        return;
    }

    let _ = tx.send(Ok(fmt.sample_rate));

    let timer_guard = unsafe {
        CreateWaitableTimerExW(
            None,
            None,
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS.0,
        )
        .ok()
        .map(|h| {
            let due_time: i64 = -50_000; // 5 ms relative
            if SetWaitableTimer(h, &due_time, 5, None, None, false).is_ok() {
                HandleGuard::new(h)
            } else {
                let _ = CloseHandle(h);
                HandleGuard::null()
            }
        })
    };
    let timer_handle = timer_guard
        .as_ref()
        .map(|g| g.raw())
        .unwrap_or(HANDLE::default());

    let stop_event = HANDLE(stop_event_raw as *mut _);
    let mut stereo_buf: Vec<f32> = Vec::with_capacity(4096);
    let mut silence_buf: Vec<f32> = Vec::new();

    let (mut last_real_qpc, qpc_freq) = query_qpc();
    let mut last_silence_qpc = last_real_qpc;
    let mut stream_lost = false;

    while !stop_flag.load(Ordering::Acquire) {
        let mut got_real_packet = false;

        loop {
            let packet_len = match unsafe { capture_client.GetNextPacketSize() } {
                Ok(len) => len,
                Err(_) => {
                    stream_lost = true;
                    break;
                }
            };
            if packet_len == 0 {
                break;
            }

            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            if unsafe {
                capture_client.GetBuffer(&mut data_ptr, &mut frames, &mut flags, None, None)
            }
            .is_err()
            {
                stream_lost = true;
                break;
            }

            if frames > 0 {
                got_real_packet = true;
                let sample_count = (frames as usize) * 2;
                if (flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)) != 0 || data_ptr.is_null() {
                    if stereo_buf.len() != sample_count {
                        stereo_buf.resize(sample_count, 0.0);
                    } else {
                        stereo_buf.fill(0.0);
                    }
                    broadcast_to_sinks(&sinks, &stereo_buf);
                } else {
                    let byte_len = (frames as usize) * (fmt.block_align as usize);
                    let raw = unsafe { std::slice::from_raw_parts(data_ptr, byte_len) };
                    decode_to_stereo_f32(raw, &fmt, &mut stereo_buf);
                    broadcast_to_sinks(&sinks, &stereo_buf);
                }
            }

            if unsafe { capture_client.ReleaseBuffer(frames) }.is_err() {
                stream_lost = true;
                break;
            }
        }

        if stream_lost {
            break;
        }

        let (now_qpc, _) = query_qpc();
        if got_real_packet {
            last_real_qpc = now_qpc;
            last_silence_qpc = now_qpc;
        } else {
            // Windows WASAPI loopback pauses packet delivery when no app produces audio.
            // Synthesize silence after 20 ms idle so delayed sinks drain tails cleanly.
            let idle_ms = ((now_qpc - last_real_qpc) as f64) * 1000.0 / (qpc_freq as f64);
            if idle_ms > 20.0 {
                let elapsed_secs =
                    ((now_qpc - last_silence_qpc) as f64 / (qpc_freq as f64)).clamp(0.0, 0.020);
                let frames = (elapsed_secs * (fmt.sample_rate as f64)).round() as usize;
                if frames > 0 {
                    silence_buf.resize(frames * 2, 0.0);
                    silence_buf.fill(0.0);
                    broadcast_to_sinks(&sinks, &silence_buf);
                    last_silence_qpc = now_qpc;
                }
            } else {
                last_silence_qpc = now_qpc;
            }
        }

        if !timer_handle.is_invalid() && !timer_handle.0.is_null() {
            let handles = [stop_event, timer_handle];
            let wait_res = unsafe { WaitForMultipleObjects(&handles, false, 20) };
            if wait_res == WAIT_OBJECT_0 {
                break;
            }
        } else {
            let wait_res = unsafe { WaitForSingleObject(stop_event, 5) };
            if wait_res == WAIT_OBJECT_0 {
                break;
            }
        }
    }

    unsafe {
        let _ = audio_client.Stop();
    }

    if stream_lost && !stop_flag.load(Ordering::Acquire) {
        on_lost();
    }
}
