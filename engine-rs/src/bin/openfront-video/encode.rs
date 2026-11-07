//! H.264 encoding (OpenH264, compiled in) into an MP4 file.

use std::io::Cursor;

use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, RateControlMode, SpsPpsStrategy, VuiConfig,
};
use openh264::formats::YUVSlices;
use openh264::OpenH264API;

pub struct VideoWriter {
    path: String,
    encoder: Encoder,
    mp4: Option<mp4::Mp4Writer<Cursor<Vec<u8>>>>,
    buf: Option<Cursor<Vec<u8>>>,
    w: usize,
    h: usize,
    fps: u32,
    yuv: Vec<u8>,
    prev_rgba: Vec<u8>,
    frames: u64,
    /// Duration written so far, in frames.
    written: u64,
    /// The last encoded frame, held back until its duration is known.
    pending: Option<(Vec<u8>, bool, u32)>,
}

/// Splits Annex B output into NAL units (without start codes).
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| {
            let mut end = starts.get(k + 1).map(|&n| n - 3).unwrap_or(data.len());
            // A 4-byte start code leaves a zero before the next 3-byte one.
            while end > s && data[end - 1] == 0 && k + 1 < starts.len() {
                end -= 1;
            }
            &data[s..end]
        })
        .collect()
}

impl VideoWriter {
    /// `bitrate` in bits per second; `threads` encoder threads.
    pub fn create(path: &str, w: usize, h: usize, fps: u32, bitrate: u32, threads: u16) -> Result<VideoWriter, String> {
        let mut config = EncoderConfig::new()
            .rate_control_mode(RateControlMode::Quality)
            .bitrate(BitRate::from_bps(bitrate))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .skip_frames(false)
            .complexity(Complexity::Low)
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .debug(false)
            // Several threads need several slices per frame.
            .num_threads(threads)
            .vui(VuiConfig::bt709());
        if threads > 1 {
            config = config.max_slice_len(60_000);
        }
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config).map_err(|e| format!("starting the encoder: {e}"))?;
        let buf = Cursor::new(Vec::with_capacity(8 << 20));
        Ok(VideoWriter {
            path: path.to_string(),
            encoder,
            mp4: None,
            buf: Some(buf),
            w,
            h,
            fps,
            yuv: vec![0; w * h * 3 / 2],
            prev_rgba: Vec::new(),
            frames: 0,
            written: 0,
            pending: None,
        })
    }

    /// Encodes an RGBA frame displayed for `repeat` frame durations.
    pub fn write(&mut self, rgba: &[u8], repeat: u32) -> Result<(), String> {
        if repeat == 0 {
            return Ok(());
        }
        rgba_to_i420_delta(rgba, &mut self.prev_rgba, self.w, self.h, &mut self.yuv);
        if repeat <= 2 {
            self.encode_yuv(repeat)
        } else {
            // Encode first frame, then one refining P-frame held for the remaining duration.
            self.encode_yuv(1)?;
            self.encode_yuv(repeat - 1)
        }
    }

    fn encode_yuv(&mut self, duration: u32) -> Result<(), String> {
        let (w, h) = (self.w, self.h);
        let (y, uv) = self.yuv.split_at(w * h);
        let (u, v) = uv.split_at(w * h / 4);
        let src = YUVSlices::new((y, u, v), (w, h), (w, w / 2, w / 2));
        let annexb = self.encoder.encode(&src).map_err(|e| format!("encoding frame {}: {e}", self.frames))?.to_vec();
        self.frames += 1;

        let mut sps = None;
        let mut pps = None;
        let mut sample = Vec::with_capacity(annexb.len() + 64);
        let mut sync = false;
        for nal in nal_units(&annexb) {
            if nal.is_empty() {
                continue;
            }
            match nal[0] & 0x1f {
                7 => sps = Some(nal.to_vec()),
                8 => pps = Some(nal.to_vec()),
                t => {
                    sync |= t == 5;
                    sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    sample.extend_from_slice(nal);
                }
            }
        }
        if self.mp4.is_none() {
            let (Some(sps), Some(pps)) = (sps, pps) else {
                return Err("the encoder's first frame has no parameter sets".into());
            };
            let mut mp4 = mp4::Mp4Writer::write_start(
                self.buf.take().unwrap(),
                &mp4::Mp4Config {
                    major_brand: str::parse("isom").unwrap(),
                    minor_version: 512,
                    compatible_brands: ["isom", "iso2", "avc1", "mp41"].iter().map(|b| str::parse(b).unwrap()).collect(),
                    timescale: 1000,
                },
            )
            .map_err(|e| e.to_string())?;
            mp4.add_track(&mp4::TrackConfig {
                track_type: mp4::TrackType::Video,
                timescale: self.fps,
                language: "und".into(),
                media_conf: mp4::MediaConfig::AvcConfig(mp4::AvcConfig {
                    width: self.w as u16,
                    height: self.h as u16,
                    seq_param_set: sps,
                    pic_param_set: pps,
                }),
            })
            .map_err(|e| e.to_string())?;
            self.mp4 = Some(mp4);
        }
        if sample.is_empty() {
            // A skipped frame: the previous one shows for longer.
            if let Some(p) = &mut self.pending {
                p.2 += duration;
            }
            return Ok(());
        }
        self.flush_pending()?;
        self.pending = Some((sample, sync, duration));
        Ok(())
    }

    fn flush_pending(&mut self) -> Result<(), String> {
        if let Some((bytes, is_sync, duration)) = self.pending.take() {
            let mp4 = self.mp4.as_mut().unwrap();
            let start_time = self.written;
            mp4.write_sample(
                1,
                &mp4::Mp4Sample { start_time, duration, rendering_offset: 0, is_sync, bytes: bytes.into() },
            )
            .map_err(|e| e.to_string())?;
            self.written += duration as u64;
        }
        Ok(())
    }

    /// Finishes the file; returns the number of frames written.
    pub fn finish(mut self) -> Result<u64, String> {
        self.flush_pending()?;
        let mut mp4 = self.mp4.take().ok_or("no frames were encoded")?;
        mp4.write_end().map_err(|e| e.to_string())?;
        let data = mp4.into_writer().into_inner();
        // The index goes first so players can start before the whole file is in.
        let out = faststart(&data).unwrap_or(data);
        std::fs::write(&self.path, out).map_err(|e| format!("writing {}: {e}", self.path))?;
        Ok(self.written)
    }
}

/// Boxes between `from` and `to`: (type, start, length).
fn boxes(data: &[u8], from: usize, to: usize) -> Option<Vec<([u8; 4], usize, usize)>> {
    let mut out = Vec::new();
    let mut i = from;
    while i + 8 <= to {
        let size = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = data[i + 4..i + 8].try_into().unwrap();
        let len = match size {
            0 => to - i,
            1 => u64::from_be_bytes(data.get(i + 8..i + 16)?.try_into().unwrap()) as usize,
            n => n,
        };
        if len < 8 || i + len > to {
            return None;
        }
        out.push((kind, i, len));
        i += len;
    }
    Some(out)
}

/// Adds `delta` to every chunk offset (stco, co64) inside a moov box.
fn shift_chunk_offsets(moov: &mut [u8], from: usize, to: usize, delta: u64) -> Option<()> {
    for (kind, start, len) in boxes(moov, from, to)? {
        match &kind {
            b"trak" | b"mdia" | b"minf" | b"stbl" => shift_chunk_offsets(moov, start + 8, start + len, delta)?,
            b"stco" => {
                let n = u32::from_be_bytes(moov[start + 12..start + 16].try_into().unwrap()) as usize;
                for k in 0..n {
                    let at = start + 16 + 4 * k;
                    let v = u32::from_be_bytes(moov[at..at + 4].try_into().unwrap()) as u64 + delta;
                    moov[at..at + 4].copy_from_slice(&u32::try_from(v).ok()?.to_be_bytes());
                }
            }
            b"co64" => {
                let n = u32::from_be_bytes(moov[start + 12..start + 16].try_into().unwrap()) as usize;
                for k in 0..n {
                    let at = start + 16 + 8 * k;
                    let v = u64::from_be_bytes(moov[at..at + 8].try_into().unwrap()) + delta;
                    moov[at..at + 8].copy_from_slice(&v.to_be_bytes());
                }
            }
            _ => {}
        }
    }
    Some(())
}

/// The file with its moov box moved in front of the media data (like
/// ffmpeg's -movflags +faststart), or None if it already is or can't be read.
fn faststart(data: &[u8]) -> Option<Vec<u8>> {
    let top = boxes(data, 0, data.len())?;
    let moov = top.iter().position(|b| &b.0 == b"moov")?;
    let mdat = top.iter().position(|b| &b.0 == b"mdat")?;
    if moov < mdat {
        return None;
    }
    let (_, ms, ml) = top[moov];
    let mut moov_bytes = data[ms..ms + ml].to_vec();
    shift_chunk_offsets(&mut moov_bytes, 8, ml, ml as u64)?;
    let mut out = Vec::with_capacity(data.len());
    for (i, &(_, start, len)) in top.iter().enumerate() {
        if i == mdat {
            out.extend_from_slice(&moov_bytes);
        }
        if i != moov {
            out.extend_from_slice(&data[start..start + len]);
        }
    }
    Some(out)
}

/// RGBA to I420 with BT.709 limited-range coefficients (as tagged in the
/// stream's VUI), skipping 32x2 pixel strips unchanged from the previous frame.
fn rgba_to_i420_delta(rgba: &[u8], prev_rgba: &mut Vec<u8>, w: usize, h: usize, out: &mut [u8]) {
    let has_prev = prev_rgba.len() == rgba.len();
    if !has_prev {
        prev_rgba.clear();
        prev_rgba.extend_from_slice(rgba);
    }
    let (y_plane, uv) = out.split_at_mut(w * h);
    let (u_plane, v_plane) = uv.split_at_mut(w * h / 4);
    // Fixed point, 16 fractional bits: Y = 16 + 219/255 (0.2126 R + 0.7152 G + 0.0722 B), etc.
    const YR: i32 = 11966;
    const YG: i32 = 40254;
    const YB: i32 = 4064;
    const UR: i32 = -6596;
    const UG: i32 = -22189;
    const UB: i32 = 28785;
    const VR: i32 = 28785;
    const VG: i32 = -26145;
    const VB: i32 = -2640;
    let half_w = w / 2;
    let pairs = h / 2;
    const STRIP_QUADS: usize = 16; // 32 pixels = 128 bytes per row
    for p in 0..pairs {
        let r0_start = (p * 2) * w * 4;
        let r1_start = (p * 2 + 1) * w * 4;
        let r1_end = (p * 2 + 2) * w * 4;
        let row0_rgba = &rgba[r0_start..r1_start];
        let row1_rgba = &rgba[r1_start..r1_end];
        if has_prev && row0_rgba == &prev_rgba[r0_start..r1_start] && row1_rgba == &prev_rgba[r1_start..r1_end] {
            continue;
        }
        let (y_row0, y_row1) = y_plane[(p * 2) * w..(p * 2 + 2) * w].split_at_mut(w);
        let u_row = &mut u_plane[p * half_w..(p + 1) * half_w];
        let v_row = &mut v_plane[p * half_w..(p + 1) * half_w];
        let mut x2 = 0;
        while x2 < half_w {
            let x2_end = (x2 + STRIP_QUADS).min(half_w);
            let b0 = x2 * 8;
            let b1 = x2_end * 8;
            if has_prev {
                let p0 = &mut prev_rgba[r0_start + b0..r0_start + b1];
                let s0 = &row0_rgba[b0..b1];
                let same0 = p0 == s0;
                if !same0 {
                    p0.copy_from_slice(s0);
                }
                let p1 = &mut prev_rgba[r1_start + b0..r1_start + b1];
                let s1 = &row1_rgba[b0..b1];
                let same1 = p1 == s1;
                if !same1 {
                    p1.copy_from_slice(s1);
                }
                if same0 && same1 {
                    x2 = x2_end;
                    continue;
                }
            }
            for q in x2..x2_end {
                let i = q * 8;
                let (r00, g00, b00) = (row0_rgba[i] as i32, row0_rgba[i + 1] as i32, row0_rgba[i + 2] as i32);
                let (r01, g01, b01) = (row0_rgba[i + 4] as i32, row0_rgba[i + 5] as i32, row0_rgba[i + 6] as i32);
                let (r10, g10, b10) = (row1_rgba[i] as i32, row1_rgba[i + 1] as i32, row1_rgba[i + 2] as i32);
                let (r11, g11, b11) = (row1_rgba[i + 4] as i32, row1_rgba[i + 5] as i32, row1_rgba[i + 6] as i32);

                y_row0[q * 2] = (16 + ((YR * r00 + YG * g00 + YB * b00 + 32768) >> 16)) as u8;
                y_row0[q * 2 + 1] = (16 + ((YR * r01 + YG * g01 + YB * b01 + 32768) >> 16)) as u8;
                y_row1[q * 2] = (16 + ((YR * r10 + YG * g10 + YB * b10 + 32768) >> 16)) as u8;
                y_row1[q * 2 + 1] = (16 + ((YR * r11 + YG * g11 + YB * b11 + 32768) >> 16)) as u8;

                let rs = r00 + r01 + r10 + r11;
                let gs = g00 + g01 + g10 + g11;
                let bs = b00 + b01 + b10 + b11;
                u_row[q] = (128 + ((UR * rs + UG * gs + UB * bs + 131072) >> 18)) as u8;
                v_row[q] = (128 + ((VR * rs + VG * gs + VB * bs + 131072) >> 18)) as u8;
            }
            x2 = x2_end;
        }
    }
}
