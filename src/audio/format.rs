use windows::core::GUID;
use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0};

const WAVE_FORMAT_PCM_TAG: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT_TAG: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE_TAG: u16 = 0xFFFE;

// KSDATAFORMAT_SUBTYPE_PCM: {00000001-0000-0010-8000-00aa00389b71}
const SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
// KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: {00000003-0000-0010-8000-00aa00389b71}
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleEncoding {
    Float32,
    Int16,
    Int24Packed,
    Int32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormatInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub block_align: u16,
    pub encoding: SampleEncoding,
}

impl AudioFormatInfo {
    pub fn bytes_per_sample(&self) -> usize {
        match self.encoding {
            SampleEncoding::Float32 | SampleEncoding::Int32 => 4,
            SampleEncoding::Int16 => 2,
            SampleEncoding::Int24Packed => 3,
        }
    }
}

/// Parses a Windows `WAVEFORMATEX` / `WAVEFORMATEXTENSIBLE` pointer safely using
/// unaligned reads (these structs are `#[repr(C, packed(1))]`).
pub unsafe fn parse_waveformat(wfx: *const WAVEFORMATEX) -> Result<AudioFormatInfo, String> {
    if wfx.is_null() {
        return Err("Null WAVEFORMATEX pointer".to_string());
    }

    let base = std::ptr::read_unaligned(wfx);
    let format_tag = base.wFormatTag;
    let channels = base.nChannels;
    let sample_rate = base.nSamplesPerSec;
    let bits_per_sample = base.wBitsPerSample;
    let block_align = base.nBlockAlign;
    let cb_size = base.cbSize;

    if channels == 0 || sample_rate == 0 || block_align == 0 {
        return Err(format!(
            "Invalid audio format parameters: ch={channels}, rate={sample_rate}, align={block_align}"
        ));
    }

    let encoding = match format_tag {
        WAVE_FORMAT_IEEE_FLOAT_TAG => {
            if bits_per_sample == 32 {
                SampleEncoding::Float32
            } else {
                return Err(format!("Unsupported IEEE float bit depth: {bits_per_sample}"));
            }
        }
        WAVE_FORMAT_PCM_TAG => match bits_per_sample {
            16 => SampleEncoding::Int16,
            24 => SampleEncoding::Int24Packed,
            32 => SampleEncoding::Int32,
            _ => return Err(format!("Unsupported PCM bit depth: {bits_per_sample}")),
        },
        WAVE_FORMAT_EXTENSIBLE_TAG => {
            if cb_size < 22 {
                return Err(format!("WAVEFORMATEXTENSIBLE cbSize too small: {cb_size}"));
            }
            let ext = std::ptr::read_unaligned(wfx as *const WAVEFORMATEXTENSIBLE);
            let subformat = ext.SubFormat;
            if subformat == SUBTYPE_IEEE_FLOAT {
                if bits_per_sample == 32 {
                    SampleEncoding::Float32
                } else {
                    return Err(format!(
                        "Unsupported extensible float bit depth: {bits_per_sample}"
                    ));
                }
            } else if subformat == SUBTYPE_PCM {
                match bits_per_sample {
                    16 => SampleEncoding::Int16,
                    24 => SampleEncoding::Int24Packed,
                    32 => SampleEncoding::Int32,
                    _ => {
                        return Err(format!(
                            "Unsupported extensible PCM bit depth: {bits_per_sample}"
                        ))
                    }
                }
            } else {
                return Err(format!("Unsupported WAVEFORMATEXTENSIBLE SubFormat: {subformat:?}"));
            }
        }
        other => return Err(format!("Unsupported wFormatTag: 0x{other:04x}")),
    };

    let expected_min_align = channels as usize * match encoding {
        SampleEncoding::Float32 | SampleEncoding::Int32 => 4,
        SampleEncoding::Int16 => 2,
        SampleEncoding::Int24Packed => 3,
    };
    if (block_align as usize) < expected_min_align {
        return Err(format!(
            "Invalid block_align {block_align} < expected {expected_min_align}"
        ));
    }

    Ok(AudioFormatInfo {
        sample_rate,
        channels,
        block_align,
        encoding,
    })
}

pub fn make_stereo_f32_waveformat(sample_rate: u32) -> WAVEFORMATEXTENSIBLE {
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE_TAG,
            nChannels: 2,
            nSamplesPerSec: sample_rate,
            nAvgBytesPerSec: sample_rate.saturating_mul(8),
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: 32,
        },
        dwChannelMask: 0x3, // SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
        SubFormat: SUBTYPE_IEEE_FLOAT,
    }
}

#[inline]
fn read_sample_f32(frame: &[u8], ch: usize, fmt: &AudioFormatInfo) -> f32 {
    let bps = fmt.bytes_per_sample();
    let off = ch * bps;
    if off + bps > frame.len() {
        return 0.0;
    }
    match fmt.encoding {
        SampleEncoding::Float32 => {
            let s = f32::from_le_bytes([frame[off], frame[off + 1], frame[off + 2], frame[off + 3]]);
            if s.is_finite() {
                s
            } else {
                0.0
            }
        }
        SampleEncoding::Int16 => {
            let v = i16::from_le_bytes([frame[off], frame[off + 1]]);
            (v as f32) / 32768.0
        }
        SampleEncoding::Int24Packed => {
            let b0 = frame[off] as i32;
            let b1 = (frame[off + 1] as i32) << 8;
            let b2 = ((frame[off + 2] as i8) as i32) << 16;
            let v = b0 | b1 | b2;
            (v as f32) / 8_388_608.0
        }
        SampleEncoding::Int32 => {
            let v = i32::from_le_bytes([frame[off], frame[off + 1], frame[off + 2], frame[off + 3]]);
            (v as f32) / 2_147_483_648.0
        }
    }
}

#[inline]
fn write_sample_f32(frame: &mut [u8], ch: usize, fmt: &AudioFormatInfo, sample: f32) {
    let bps = fmt.bytes_per_sample();
    let off = ch * bps;
    if off + bps > frame.len() {
        return;
    }
    let s = if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    match fmt.encoding {
        SampleEncoding::Float32 => {
            frame[off..off + 4].copy_from_slice(&s.to_le_bytes());
        }
        SampleEncoding::Int16 => {
            let scaled = (s * 32767.0).round().clamp(-32768.0, 32767.0) as i16;
            frame[off..off + 2].copy_from_slice(&scaled.to_le_bytes());
        }
        SampleEncoding::Int24Packed => {
            let scaled = (s * 8_388_607.0)
                .round()
                .clamp(-8_388_608.0, 8_388_607.0) as i32;
            let bytes = scaled.to_le_bytes();
            frame[off] = bytes[0];
            frame[off + 1] = bytes[1];
            frame[off + 2] = bytes[2];
        }
        SampleEncoding::Int32 => {
            let scaled = (s as f64 * 2_147_483_647.0)
                .round()
                .clamp(-2_147_483_648.0, 2_147_483_647.0) as i32;
            frame[off..off + 4].copy_from_slice(&scaled.to_le_bytes());
        }
    }
}

pub fn decode_to_stereo_f32(raw: &[u8], fmt: &AudioFormatInfo, out: &mut Vec<f32>) {
    out.clear();
    let align = fmt.block_align as usize;
    if align == 0 || fmt.channels == 0 {
        return;
    }
    let frame_count = raw.len() / align;
    out.reserve(frame_count * 2);

    let ch = fmt.channels as usize;
    for i in 0..frame_count {
        let frame = &raw[i * align..(i + 1) * align];
        let (l, r) = match ch {
            1 => {
                let m = read_sample_f32(frame, 0, fmt);
                (m, m)
            }
            2 => (
                read_sample_f32(frame, 0, fmt),
                read_sample_f32(frame, 1, fmt),
            ),
            4 => {
                let fl = read_sample_f32(frame, 0, fmt);
                let fr = read_sample_f32(frame, 1, fmt);
                let bl = read_sample_f32(frame, 2, fmt);
                let br = read_sample_f32(frame, 3, fmt);
                (
                    (fl + 0.5 * bl).clamp(-1.0, 1.0),
                    (fr + 0.5 * br).clamp(-1.0, 1.0),
                )
            }
            n if n >= 6 => {
                // Standard Windows channel order: FL, FR, FC, LFE, BL, BR, (SL, SR)
                let fl = read_sample_f32(frame, 0, fmt);
                let fr = read_sample_f32(frame, 1, fmt);
                let fc = read_sample_f32(frame, 2, fmt);
                let bl = read_sample_f32(frame, 4, fmt);
                let br = read_sample_f32(frame, 5, fmt);
                let sl = if n >= 8 { read_sample_f32(frame, 6, fmt) } else { 0.0 };
                let sr = if n >= 8 { read_sample_f32(frame, 7, fmt) } else { 0.0 };
                let l = fl + 0.70710677 * fc + 0.5 * bl + 0.5 * sl;
                let r = fr + 0.70710677 * fc + 0.5 * br + 0.5 * sr;
                (l.clamp(-1.0, 1.0), r.clamp(-1.0, 1.0))
            }
            _ => {
                let fl = read_sample_f32(frame, 0, fmt);
                let fr = read_sample_f32(frame, 1, fmt);
                let fc = read_sample_f32(frame, 2, fmt);
                (
                    (fl + 0.70710677 * fc).clamp(-1.0, 1.0),
                    (fr + 0.70710677 * fc).clamp(-1.0, 1.0),
                )
            }
        };
        out.push(l);
        out.push(r);
    }
}

pub fn encode_from_stereo_f32(stereo: &[f32], fmt: &AudioFormatInfo, out: &mut [u8]) {
    let align = fmt.block_align as usize;
    if align == 0 || fmt.channels == 0 {
        return;
    }
    let frames = (stereo.len() / 2).min(out.len() / align);
    let ch = fmt.channels as usize;

    for i in 0..frames {
        let l = stereo[i * 2];
        let r = stereo[i * 2 + 1];
        let frame = &mut out[i * align..(i + 1) * align];
        frame.fill(0);
        if ch == 1 {
            write_sample_f32(frame, 0, fmt, 0.5 * (l + r));
        } else {
            write_sample_f32(frame, 0, fmt, l);
            write_sample_f32(frame, 1, fmt, r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stereo_f32_extensible() {
        let wfx = make_stereo_f32_waveformat(48_000);
        let parsed = unsafe { parse_waveformat(&wfx.Format as *const WAVEFORMATEX) }
            .expect("should parse stereo f32");
        assert_eq!(parsed.sample_rate, 48_000);
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.block_align, 8);
        assert_eq!(parsed.encoding, SampleEncoding::Float32);
    }

    #[test]
    fn decode_encode_float32_and_int16() {
        let fmt_f32 = AudioFormatInfo {
            sample_rate: 48_000,
            channels: 2,
            block_align: 8,
            encoding: SampleEncoding::Float32,
        };
        let input = [0.25f32, -0.75f32, 0.5f32, 0.0f32];
        let mut raw_f32 = vec![0u8; 16];
        encode_from_stereo_f32(&input, &fmt_f32, &mut raw_f32);
        let mut decoded = Vec::new();
        decode_to_stereo_f32(&raw_f32, &fmt_f32, &mut decoded);
        assert_eq!(decoded, input);

        let fmt_i16 = AudioFormatInfo {
            sample_rate: 48_000,
            channels: 2,
            block_align: 4,
            encoding: SampleEncoding::Int16,
        };
        let mut raw_i16 = vec![0u8; 8];
        encode_from_stereo_f32(&input, &fmt_i16, &mut raw_i16);
        decode_to_stereo_f32(&raw_i16, &fmt_i16, &mut decoded);
        for (a, b) in decoded.iter().zip(input.iter()) {
            assert!((a - b).abs() < 0.001, "expected {b}, got {a}");
        }
    }

    #[test]
    fn decode_int24_and_int32() {
        let fmt_24 = AudioFormatInfo {
            sample_rate: 48_000,
            channels: 1,
            block_align: 3,
            encoding: SampleEncoding::Int24Packed,
        };
        // +0.5 full scale 24-bit = 0x400000
        let raw_24 = [0x00u8, 0x00, 0x40];
        let mut out = Vec::new();
        decode_to_stereo_f32(&raw_24, &fmt_24, &mut out);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.5).abs() < 0.0001);
        assert!((out[1] - 0.5).abs() < 0.0001);

        let fmt_32 = AudioFormatInfo {
            sample_rate: 48_000,
            channels: 1,
            block_align: 4,
            encoding: SampleEncoding::Int32,
        };
        let val = (-0.5f64 * 2_147_483_648.0) as i32;
        let raw_32 = val.to_le_bytes();
        decode_to_stereo_f32(&raw_32, &fmt_32, &mut out);
        assert_eq!(out.len(), 2);
        assert!((out[0] + 0.5).abs() < 0.0001);
        assert!((out[1] + 0.5).abs() < 0.0001);
    }

    #[test]
    fn decode_5_1_downmix() {
        let fmt_51 = AudioFormatInfo {
            sample_rate: 48_000,
            channels: 6,
            block_align: 24,
            encoding: SampleEncoding::Float32,
        };
        // FL=0.2, FR=0.1, FC=0.5, LFE=0.9 (ignored), BL=0.2, BR=0.4
        let samples = [0.2f32, 0.1, 0.5, 0.9, 0.2, 0.4];
        let mut raw = Vec::with_capacity(24);
        for s in samples {
            raw.extend_from_slice(&s.to_le_bytes());
        }
        let mut out = Vec::new();
        decode_to_stereo_f32(&raw, &fmt_51, &mut out);
        assert_eq!(out.len(), 2);
        let expected_l = 0.2 + 0.70710677 * 0.5 + 0.5 * 0.2;
        let expected_r = 0.1 + 0.70710677 * 0.5 + 0.5 * 0.4;
        assert!((out[0] - expected_l).abs() < 1e-5);
        assert!((out[1] - expected_r).abs() < 1e-5);
    }
}
