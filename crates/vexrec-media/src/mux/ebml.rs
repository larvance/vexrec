use std::io::{BufWriter, Seek, SeekFrom, Write};
use vexrec_core::error::Result;
use vexrec_core::formats::ContainerFormat;

use super::bitstream::{annexb_to_length_prefixed, build_avcc_record, build_opus_head_matroska, extract_h264_sps_pps};

struct EbmlWriter {
    buf: Vec<u8>,
}

impl EbmlWriter {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(4096),
        }
    }

    fn write_id(&mut self, id: u32) {
        let bytes = id.to_be_bytes();
        let start = bytes.iter().position(|&b| b != 0).unwrap_or(3);
        self.buf.extend_from_slice(&bytes[start..]);
    }

    fn write_vint_size(&mut self, val: u64) {
        if val < 0x7f {
            self.buf.push(0x80 | val as u8);
        } else if val < 0x3fff {
            let v = 0x4000 | (val as u16);
            self.buf.extend_from_slice(&v.to_be_bytes());
        } else if val < 0x1f_ffff {
            let v = 0x20_0000 | (val as u32);
            self.buf.extend_from_slice(&v.to_be_bytes()[1..4]);
        } else {
            let v = 0x1000_0000 | (val as u32);
            self.buf.extend_from_slice(&v.to_be_bytes());
        }
    }

    fn begin_master_4(&mut self, id: u32) -> usize {
        self.write_id(id);
        let pos = self.buf.len();
        self.buf.extend_from_slice(&[0x10, 0x00, 0x00, 0x00]); // 4-byte placeholder VINT
        pos
    }

    fn end_master_4(&mut self, size_pos: usize) {
        let payload_len = (self.buf.len() - size_pos - 4) as u32;
        let v = 0x1000_0000u32 | payload_len;
        self.buf[size_pos..size_pos + 4].copy_from_slice(&v.to_be_bytes());
    }

    fn write_uint(&mut self, id: u32, val: u64) {
        self.write_id(id);
        let bytes = val.to_be_bytes();
        let first_nz = bytes.iter().position(|&b| b != 0).unwrap_or(7);
        let slice = &bytes[first_nz..];
        self.write_vint_size(slice.len() as u64);
        self.buf.extend_from_slice(slice);
    }

    fn write_f64(&mut self, id: u32, val: f64) {
        self.write_id(id);
        self.write_vint_size(8);
        self.buf.extend_from_slice(&val.to_bits().to_be_bytes());
    }

    fn write_str(&mut self, id: u32, s: &str) {
        self.write_id(id);
        self.write_vint_size(s.len() as u64);
        self.buf.extend_from_slice(s.as_bytes());
    }

    fn write_bytes(&mut self, id: u32, b: &[u8]) {
        self.write_id(id);
        self.write_vint_size(b.len() as u64);
        self.buf.extend_from_slice(b);
    }
}

/// Pure-Rust streaming Matroska (`.mkv`) / WebM (`.webm`) muxer.
pub struct EbmlMuxer<W: Write + Seek> {
    writer: BufWriter<W>,
    is_webm: bool,
    width: u32,
    height: u32,
    fps: u32,
    audio_channels: u8,
    audio_enabled: bool,
    opus_pre_skip: u16,
    header_written: bool,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    segment_size_file_pos: u64,
    segment_data_start: u64,
    cluster_size_file_pos: Option<u64>,
    cluster_ts_ms: u64,
    in_cluster: bool,
    max_ts_ms: u64,
    last_track_ts_ms: [Option<u64>; 2],
    duration_file_pos: Option<u64>,
    pending_video: Vec<(u64, bool, Vec<u8>)>,
    pending_audio: Vec<(u64, Vec<u8>)>,
}

impl<W: Write + Seek> EbmlMuxer<W> {
    pub fn new(
        writer: W,
        container: ContainerFormat,
        width: u32,
        height: u32,
        fps: u32,
        audio_channels: u8,
        audio_enabled: bool,
        opus_pre_skip: u16,
    ) -> Result<Self> {
        let mut s = Self {
            writer: BufWriter::with_capacity(1024 * 1024, writer),
            is_webm: container == ContainerFormat::Webm,
            width,
            height,
            fps: fps.max(1),
            audio_channels,
            audio_enabled,
            opus_pre_skip,
            header_written: false,
            sps: None,
            pps: None,
            segment_size_file_pos: 0,
            segment_data_start: 0,
            cluster_size_file_pos: None,
            cluster_ts_ms: 0,
            in_cluster: false,
            max_ts_ms: 0,
            last_track_ts_ms: [None, None],
            duration_file_pos: None,
            pending_video: Vec::new(),
            pending_audio: Vec::new(),
        };
        s.write_ebml_header()?;
        Ok(s)
    }

    pub fn set_h264_params(&mut self, sps: Vec<u8>, pps: Vec<u8>) {
        self.sps = Some(sps);
        self.pps = Some(pps);
    }

    fn write_ebml_header(&mut self) -> Result<()> {
        let mut w = EbmlWriter::new();
        let hdr = w.begin_master_4(0x1a45_dfa3);
        w.write_uint(0x4286, 1); // EBMLVersion
        w.write_uint(0x42f7, 1); // EBMLReadVersion
        w.write_uint(0x42f2, 4); // EBMLMaxIDLength
        w.write_uint(0x42f3, 8); // EBMLMaxSizeLength
        w.write_str(0x4282, if self.is_webm { "webm" } else { "matroska" });
        w.write_uint(0x4287, 4); // DocTypeVersion
        w.write_uint(0x4285, 2); // DocTypeReadVersion
        w.end_master_4(hdr);
        self.writer.write_all(&w.buf)?;
        Ok(())
    }

    fn write_segment_header_if_ready(&mut self) -> Result<()> {
        if self.header_written {
            return Ok(());
        }
        // Segment Element ID + 8-byte unknown size placeholder (patched on finish)
        self.writer.write_all(&0x1853_8067u32.to_be_bytes())?;
        self.segment_size_file_pos = self.writer.stream_position()?;
        self.writer.write_all(&[0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff])?;
        self.segment_data_start = self.segment_size_file_pos + 8;

        let mut w = EbmlWriter::new();
        // Segment Info (0x1549A966)
        let info = w.begin_master_4(0x1549_a966);
        w.write_uint(0x2ad7_b1, 1_000_000); // TimestampScale = 1ms
        w.write_str(0x4d80, "Vexrec-Rust");
        w.write_str(0x5741, "Vexrec 0.3");
        // Duration placeholder
        w.write_id(0x4489);
        w.write_vint_size(8);
        let dur_rel = w.buf.len() as u64;
        w.buf.extend_from_slice(&0.0f64.to_bits().to_be_bytes());
        w.end_master_4(info);

        // Tracks (0x1654AE6B)
        let tracks = w.begin_master_4(0x1654_ae6b);
        {
            let tr1 = w.begin_master_4(0xae);
            w.write_uint(0xd7, 1); // TrackNumber
            w.write_uint(0x73c5, 1); // TrackUID
            w.write_uint(0x83, 1); // TrackType: Video
            w.write_uint(0x23e3_83, (1_000_000_000 / self.fps as u64).max(1)); // DefaultDuration
            w.write_str(0x86, "V_MPEG4/ISO/AVC");
            if let (Some(ref s), Some(ref p)) = (&self.sps, &self.pps) {
                let avcc = build_avcc_record(s, p);
                w.write_bytes(0x63a2, &avcc); // CodecPrivate
            }
            let vid = w.begin_master_4(0xe0);
            w.write_uint(0xb0, self.width as u64);
            w.write_uint(0xba, self.height as u64);
            let colour = w.begin_master_4(0x55b0);
            w.write_uint(0x55b1, 1); // MatrixCoefficients BT.709
            w.write_uint(0x55b9, 1); // Range Broadcast/TV limited
            w.write_uint(0x55ba, 1); // TransferCharacteristics BT.709
            w.write_uint(0x55bb, 1); // Primaries BT.709
            w.end_master_4(colour);
            w.end_master_4(vid);
            w.end_master_4(tr1);
        }

        if self.audio_enabled {
            let tr2 = w.begin_master_4(0xae);
            w.write_uint(0xd7, 2);
            w.write_uint(0x73c5, 2);
            w.write_uint(0x83, 2); // Audio
            w.write_str(0x86, "A_OPUS");
            let head = build_opus_head_matroska(self.audio_channels, self.opus_pre_skip, 48_000);
            w.write_bytes(0x63a2, &head);
            w.write_uint(0x56aa, 6_500_000); // CodecDelay ns
            w.write_uint(0x56bb, 80_000_000); // SeekPreRoll ns
            let aud = w.begin_master_4(0xe1);
            w.write_f64(0xb5, 48_000.0);
            w.write_uint(0x9f, self.audio_channels as u64);
            w.write_uint(0x6264, 32);
            w.end_master_4(aud);
            w.end_master_4(tr2);
        }
        w.end_master_4(tracks);

        self.duration_file_pos = Some(self.segment_data_start + dur_rel);
        self.writer.write_all(&w.buf)?;
        self.header_written = true;
        Ok(())
    }

    fn close_current_cluster(&mut self) -> Result<()> {
        if let Some(size_pos) = self.cluster_size_file_pos.take() {
            self.writer.flush()?;
            let end_pos = self.writer.stream_position()?;
            let cluster_data_start = size_pos + 8;
            if end_pos >= cluster_data_start {
                let clen = end_pos - cluster_data_start;
                let val = 0x0100_0000_0000_0000u64 | (clen & 0x00ff_ffff_ffff_ffff);
                self.writer.seek(SeekFrom::Start(size_pos))?;
                self.writer.write_all(&val.to_be_bytes())?;
                self.writer.flush()?;
                self.writer.seek(SeekFrom::Start(end_pos))?;
            }
        }
        self.in_cluster = false;
        Ok(())
    }

    fn start_new_cluster(&mut self, ts_ms: u64) -> Result<()> {
        self.close_current_cluster()?;
        self.writer.write_all(&0x1f43_b675u32.to_be_bytes())?;
        self.writer.flush()?;
        let size_pos = self.writer.stream_position()?;
        self.writer
            .write_all(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00])?;
        self.cluster_size_file_pos = Some(size_pos);

        let mut cw = EbmlWriter::new();
        cw.write_uint(0xe7, ts_ms); // Timestamp
        self.writer.write_all(&cw.buf)?;
        self.cluster_ts_ms = ts_ms;
        self.in_cluster = true;
        Ok(())
    }

    fn emit_simple_block(&mut self, track_num: u8, mut ts_ms: u64, is_key: bool, payload: &[u8]) -> Result<()> {
        let t_idx = (track_num.saturating_sub(1) as usize).min(1);
        if let Some(prev) = self.last_track_ts_ms[t_idx] {
            if ts_ms <= prev {
                ts_ms = prev + 1;
            }
        }
        self.last_track_ts_ms[t_idx] = Some(ts_ms);

        if !self.in_cluster || ts_ms.saturating_sub(self.cluster_ts_ms) > 25_000 || (is_key && ts_ms.saturating_sub(self.cluster_ts_ms) >= 1500 && track_num == 1) {
            self.start_new_cluster(ts_ms)?;
        }
        let rel = (ts_ms as i64 - self.cluster_ts_ms as i64).clamp(-32767, 32767) as i16;
        let flags: u8 = if is_key { 0x80 } else { 0x00 };
        let total_len = 1 + 2 + 1 + payload.len();

        let mut hdr = EbmlWriter::new();
        hdr.write_id(0xa3); // SimpleBlock
        hdr.write_vint_size(total_len as u64);
        hdr.buf.push(0x80 | track_num);
        hdr.buf.extend_from_slice(&rel.to_be_bytes());
        hdr.buf.push(flags);

        self.writer.write_all(&hdr.buf)?;
        self.writer.write_all(payload)?;
        if ts_ms > self.max_ts_ms {
            self.max_ts_ms = ts_ms;
        }
        Ok(())
    }

    pub fn push_video_frame(&mut self, pts_ns: u64, force_key_hint: bool, annexb: &[u8]) -> Result<()> {
        if self.sps.is_none() || self.pps.is_none() {
            let (s, p) = extract_h264_sps_pps(annexb);
            if let (Some(sps), Some(pps)) = (s, p) {
                self.sps = Some(sps);
                self.pps = Some(pps);
            }
        }

        let (avcc_sample, is_idr) = annexb_to_length_prefixed(annexb, true);
        if avcc_sample.is_empty() {
            return Ok(());
        }
        let ts_ms = pts_ns / 1_000_000;
        let key = is_idr || force_key_hint;

        if self.sps.is_none() || self.pps.is_none() {
            self.pending_video.push((ts_ms, key, avcc_sample));
            return Ok(());
        }
        self.write_segment_header_if_ready()?;

        if !self.pending_video.is_empty() {
            let pending = std::mem::take(&mut self.pending_video);
            for (p_ts, p_key, p_data) in pending {
                self.emit_simple_block(1, p_ts, p_key, &p_data)?;
            }
        }
        self.emit_simple_block(1, ts_ms, key, &avcc_sample)?;
        if !self.pending_audio.is_empty() {
            let pending_a = std::mem::take(&mut self.pending_audio);
            for (a_ts, a_data) in pending_a {
                self.emit_simple_block(2, a_ts, true, &a_data)?;
            }
        }
        Ok(())
    }

    pub fn push_audio_packet(&mut self, pts_ns: u64, opus_pkt: &[u8]) -> Result<()> {
        if !self.audio_enabled || opus_pkt.is_empty() {
            return Ok(());
        }
        let ts_ms = pts_ns / 1_000_000;
        if !self.header_written {
            self.pending_audio.push((ts_ms, opus_pkt.to_vec()));
            return Ok(());
        }
        self.emit_simple_block(2, ts_ms, true, opus_pkt)
    }

    pub fn finish(&mut self) -> Result<()> {
        if !self.header_written && self.sps.is_some() && self.pps.is_some() {
            let _ = self.write_segment_header_if_ready();
        }
        if self.header_written {
            if !self.pending_video.is_empty() {
                let pending = std::mem::take(&mut self.pending_video);
                for (p_ts, p_key, p_data) in pending {
                    let _ = self.emit_simple_block(1, p_ts, p_key, &p_data);
                }
            }
            if !self.pending_audio.is_empty() {
                let pending_a = std::mem::take(&mut self.pending_audio);
                for (a_ts, a_data) in pending_a {
                    let _ = self.emit_simple_block(2, a_ts, true, &a_data);
                }
            }
        }
        let _ = self.close_current_cluster();
        self.writer.flush()?;
        let end_pos = self.writer.stream_position()?;

        if let Some(dur_pos) = self.duration_file_pos {
            let _ = self.writer.seek(SeekFrom::Start(dur_pos));
            let _ = self.writer.write_all(&(self.max_ts_ms as f64).to_bits().to_be_bytes());
        }
        if self.segment_size_file_pos > 0 && end_pos > self.segment_data_start {
            let seg_len = end_pos - self.segment_data_start;
            let val = 0x0100_0000_0000_0000u64 | (seg_len & 0x00ff_ffff_ffff_ffff);
            let _ = self.writer.seek(SeekFrom::Start(self.segment_size_file_pos));
            let _ = self.writer.write_all(&val.to_be_bytes());
        }
        self.writer.flush()?;
        let _ = self.writer.seek(SeekFrom::Start(end_pos));
        Ok(())
    }
}
