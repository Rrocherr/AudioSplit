use crate::config::{MAX_DELAY_MS, MAX_VOLUME};

pub struct DelayRingBuffer {
    data: Vec<f32>,
    capacity_frames: usize,
    sample_rate: u32,
    write_pos: u64,
    read_pos: f64,
    ema_error: f64,
    current_gain: f32,
    fade_frames_remaining: u32,
    buffering: bool,
    jitter_cushion_frames: f64,
}

impl DelayRingBuffer {
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate.max(8_000);
        // >= 2.0 seconds capacity so max 1000 ms delay + cushion + burst margin fits comfortably.
        let capacity_frames = ((sr as usize) * 2).max(96_000);
        Self {
            data: vec![0.0; capacity_frames * 2],
            capacity_frames,
            sample_rate: sr,
            write_pos: 0,
            read_pos: 0.0,
            ema_error: 0.0,
            current_gain: 1.0,
            fade_frames_remaining: 0,
            buffering: true,
            jitter_cushion_frames: (sr as f64) * 0.020,
        }
    }

    pub fn push_frames(&mut self, stereo: &[f32]) {
        let num_frames = stereo.len() / 2;
        if num_frames == 0 {
            return;
        }

        for i in 0..num_frames {
            let slot = ((self.write_pos as usize) % self.capacity_frames) * 2;
            self.data[slot] = stereo[i * 2];
            self.data[slot + 1] = stereo[i * 2 + 1];
            self.write_pos += 1;
        }

        // Leave 2 frames margin for linear interpolation safety.
        let min_read = (self.write_pos as f64) - (self.capacity_frames as f64 - 2.0);
        if self.read_pos < min_read {
            self.read_pos = min_read;
        }
    }

    pub fn read_frames(
        &mut self,
        out_stereo: &mut [f32],
        delay_ms: u32,
        volume_pct: u32,
        rate_ratio: f64,
    ) {
        let out_frames = out_stereo.len() / 2;
        if out_frames == 0 {
            return;
        }

        let clamped_vol = volume_pct.min(MAX_VOLUME);
        let target_gain = if clamped_vol == 0 {
            0.0
        } else {
            let v = (clamped_vol as f32) / 100.0;
            v * v
        };

        let clamped_delay = delay_ms.min(MAX_DELAY_MS);
        let target_fill = (self.jitter_cushion_frames
            + (clamped_delay as f64) * (self.sample_rate as f64) / 1000.0)
            .min((self.capacity_frames as f64) - 64.0);

        let safe_ratio = if rate_ratio.is_finite() && rate_ratio > 0.1 && rate_ratio < 10.0 {
            rate_ratio
        } else {
            1.0
        };

        let fade_len = (self.sample_rate / 200).max(64);
        let max_err = 0.060 * (self.sample_rate as f64);
        let min_needed = (out_frames as f64 * safe_ratio) + 4.0;

        let mut available = (self.write_pos as f64) - self.read_pos;
        let mut error = available - target_fill;

        if error > max_err {
            // Large delay reduction or producer surge: snap read pointer cleanly.
            self.read_pos = (self.write_pos as f64) - target_fill;
            self.ema_error = 0.0;
            self.fade_frames_remaining = fade_len;
            self.buffering = false;
            available = target_fill;
            error = 0.0;
        }

        if self.buffering {
            if available >= target_fill && available >= min_needed {
                self.buffering = false;
                self.ema_error = 0.0;
                self.fade_frames_remaining = fade_len;
            } else {
                self.slew_gain_silently(out_frames, target_gain);
                out_stereo.fill(0.0);
                return;
            }
        } else if available < min_needed || error < -max_err {
            // Underrun or large delay increase: pause read_pos and let buffer refill.
            self.buffering = true;
            self.ema_error = 0.0;
            self.slew_gain_silently(out_frames, target_gain);
            out_stereo.fill(0.0);
            return;
        }

        self.ema_error = self.ema_error * 0.96 + error * 0.04;
        let trim = (self.ema_error / (self.sample_rate as f64) * 0.08).clamp(-0.005, 0.005);
        let step = safe_ratio * (1.0 + trim);

        let max_pos = (self.write_pos as f64) - 1.0001;
        let gain_slew = 0.005f32;

        for i in 0..out_frames {
            if (target_gain - self.current_gain).abs() <= gain_slew {
                self.current_gain = target_gain;
            } else if target_gain > self.current_gain {
                self.current_gain += gain_slew;
            } else {
                self.current_gain -= gain_slew;
            }

            let fade_mul = if self.fade_frames_remaining > 0 {
                let elapsed = fade_len - self.fade_frames_remaining;
                self.fade_frames_remaining -= 1;
                (elapsed as f32) / (fade_len as f32)
            } else {
                1.0
            };

            let pos = self.read_pos.min(max_pos).max(0.0);
            let idx0 = pos.floor() as u64;
            let frac = (pos - (idx0 as f64)) as f32;
            let idx1 = (idx0 + 1).min(self.write_pos.saturating_sub(1));

            let s0 = ((idx0 as usize) % self.capacity_frames) * 2;
            let s1 = ((idx1 as usize) % self.capacity_frames) * 2;

            let l0 = self.data[s0];
            let r0 = self.data[s0 + 1];
            let l1 = self.data[s1];
            let r1 = self.data[s1 + 1];

            let g = self.current_gain * fade_mul;
            let l = (l0 + (l1 - l0) * frac) * g;
            let r = (r0 + (r1 - r0) * frac) * g;

            out_stereo[i * 2] = l.clamp(-1.0, 1.0);
            out_stereo[i * 2 + 1] = r.clamp(-1.0, 1.0);

            self.read_pos += step;
        }
    }

    fn slew_gain_silently(&mut self, frames: usize, target_gain: f32) {
        let delta = 0.005f32 * (frames as f32);
        if (target_gain - self.current_gain).abs() <= delta {
            self.current_gain = target_gain;
        } else if target_gain > self.current_gain {
            self.current_gain += delta;
        } else {
            self.current_gain -= delta;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_delay_offset() {
        let sr = 48_000u32;
        let mut rb = DelayRingBuffer::new(sr);
        let chunk_frames = 480; // 10 ms
        let delay_ms = 100u32;
        let impulse_frame = 4_800usize; // pushed at 100 ms mark

        let mut total_pushed = 0usize;
        let mut out = vec![0.0f32; chunk_frames * 2];
        let mut peak_output_frame = None;

        for tick in 0..45 {
            let mut input = vec![0.0f32; chunk_frames * 2];
            for f in 0..chunk_frames {
                if total_pushed + f == impulse_frame {
                    input[f * 2] = 1.0;
                    input[f * 2 + 1] = 1.0;
                }
            }
            rb.push_frames(&input);
            total_pushed += chunk_frames;

            rb.read_frames(&mut out, delay_ms, 100, 1.0);
            for f in 0..chunk_frames {
                if out[f * 2] > 0.5 && peak_output_frame.is_none() {
                    peak_output_frame = Some(tick * chunk_frames + f);
                }
            }
        }

        // Expected arrival: impulse_frame (100ms) + jitter_cushion (20ms) + delay (100ms) = 220ms = 10560 frames
        // Allow <= 1 chunk (480 frames) quantization since buffering unblocks on chunk boundary.
        let peak = peak_output_frame.expect("impulse should appear at output");
        let expected = impulse_frame + 960 + 4800; // 10560
        assert!(
            (peak as isize - expected as isize).abs() <= chunk_frames as isize,
            "impulse arrived at {peak}, expected near {expected}"
        );
    }

    #[test]
    fn volume_scaling_and_mute() {
        let sr = 48_000u32;
        let mut rb = DelayRingBuffer::new(sr);
        let chunk = vec![0.8f32; 480 * 2];
        let mut out = vec![0.0f32; 480 * 2];

        // Fill cushion + settle gain at 50% (0.5^2 = 0.25)
        for _ in 0..20 {
            rb.push_frames(&chunk);
            rb.read_frames(&mut out, 0, 50, 1.0);
        }
        let expected = 0.8 * 0.25;
        assert!(
            (out[out.len() - 2] - expected).abs() < 0.01,
            "expected ~{expected}, got {}",
            out[out.len() - 2]
        );

        // Mute (0%)
        for _ in 0..10 {
            rb.push_frames(&chunk);
            rb.read_frames(&mut out, 0, 0, 1.0);
        }
        assert!(out[out.len() - 2].abs() < 1e-6);
    }

    #[test]
    fn wrap_around_across_ring_boundary() {
        let sr = 48_000u32;
        let mut rb = DelayRingBuffer::new(sr);
        let chunk_frames = 960; // 20 ms
        let mut out = vec![0.0f32; chunk_frames * 2];

        // Push & read > 3 seconds (exceeds 2s capacity) and confirm steady non-zero signal
        for tick in 0..200 {
            let val = if tick % 2 == 0 { 0.3f32 } else { -0.3f32 };
            let input = vec![val; chunk_frames * 2];
            rb.push_frames(&input);
            rb.read_frames(&mut out, 50, 100, 1.0);
        }

        let max_abs = out.iter().copied().fold(0.0f32, |a, b| a.max(b.abs()));
        assert!(max_abs > 0.2, "signal died after ring wrap: {max_abs}");
        assert!(out.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn adaptive_clock_drift_and_live_delay_change() {
        let sr = 48_000u32;
        let mut rb = DelayRingBuffer::new(sr);
        let push_chunk = vec![0.5f32; 480 * 2]; // 10 ms at 48000 Hz
        let mut out = vec![0.0f32; 480 * 2];

        // Simulate 0.1% faster render clock (ratio = 0.999) for 3 seconds
        for _ in 0..300 {
            rb.push_frames(&push_chunk);
            rb.read_frames(&mut out, 40, 100, 0.999);
            assert!(out.iter().all(|x| x.is_finite()));
        }
        assert!(!rb.buffering, "adaptive drift should prevent underrun");

        // Live increase delay to 300 ms, then decrease back to 0 ms
        for _ in 0..40 {
            rb.push_frames(&push_chunk);
            rb.read_frames(&mut out, 300, 100, 1.0);
        }
        for _ in 0..30 {
            rb.push_frames(&push_chunk);
            rb.read_frames(&mut out, 0, 100, 1.0);
        }
        assert!(!rb.buffering);
        assert!(out.iter().all(|x| x.is_finite()));
    }
}
