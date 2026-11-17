use std::io::{BufWriter, Seek, SeekFrom, Write};
use vexrec_core::error::{Result, VexrecError};
use vexrec_core::formats::VideoCodec;

use super::bitstream::{annexb_to_length_prefixed, build_avcc_record, build_opus_dops_mp4, extract_h264_sps_pps};

pub const VIDEO_TIMESCALE: u64 = 90_000;
pub const AUDIO_TIMESCALE: u64 = 48_000;

struct BoxCursor {
    buf: Vec<u8>,
}

impl BoxCursor {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(4096),
        }
    }

    fn begin(&mut self, tag: &[u8; 4]) -> usize {
        let pos = self.buf.len();
        self.buf.extend_from_slice(&[0u8; 4]);
        self.buf.extend_from_slice(tag);
        pos
    }

    fn begin_full(&mut self, tag: &[u8; 4], version: u8, flags: u32) -> usize {
        let pos = self.begin(tag);
        self.buf.push(version);
        let f = flags.to_be_bytes();
        self.buf.extend_from_slice(&f[1..4]);
        pos
    }

    fn end(&mut self, start_pos: usize) {
        let size = (self.buf.len() - start_pos) as u32;
        self.buf[start_pos..start_pos + 4].copy_from_slice(&size.to_be_bytes());
    }

    fn push_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    fn push_u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn push_i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn push_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn push_i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn push_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn push_zeros(&mut self, n: usize) {
        self.buf.resize(self.buf.len() + n, 0);
    }

    fn push_bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }
}

struct QueuedVideoSample {
    pts_90k: u64,
    duration_90k: u32,
    is_keyframe: bool,
    payload: Vec<u8>,
}

struct QueuedAudioSample {
    pts_48k: u64,
    duration_48k: u32,
    payload: Vec<u8>,
}

struct FragIndexEntry {
    time_90k: u64,
    moof_offset: u64,
}

/// Pure-Rust streaming Fragmented MP4 (`fMP4`) muxer emitting crash-safe `moof + mdat` segments
/// aligned on keyframes and appending `mfra` random access index on finish.
pub struct FragmentedMp4Muxer<W: Write + Seek> {
    writer: BufWriter<W>,
    width: u32,
    height: u32,
    fps: u32,
    video_codec: VideoCodec,
    audio_channels: u8,
    audio_enabled: bool,
    opus_pre_skip: u16,
    moov_written: bool,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    seq_number: u32,
    video_base_decode_90k: u64,
    audio_base_decode_48k: u64,
    last_video_pts_90k: Option<u64>,
    last_audio_pts_48k: Option<u64>,
    video_queue: Vec<QueuedVideoSample>,
    audio_queue: Vec<QueuedAudioSample>,
    frag_entries: Vec<FragIndexEntry>,
    bytes_written: u64,
}

impl<W: Write + Seek> FragmentedMp4Muxer<W> {
    pub fn new(
        writer: W,
        width: u32,
        height: u32,
        fps: u32,
        video_codec: VideoCodec,
        audio_channels: u8,
        audio_enabled: bool,
        opus_pre_skip: u16,
    ) -> Result<Self> {
        let mut s = Self {
            writer: BufWriter::with_capacity(1024 * 1024, writer),
            width,
            height,
            fps: fps.max(1),
            video_codec,
            audio_channels,
            audio_enabled,
            opus_pre_skip,
            moov_written: false,
            sps: None,
            pps: None,
            seq_number: 1,
            video_base_decode_90k: 0,
            audio_base_decode_48k: 0,
            last_video_pts_90k: None,
            last_audio_pts_48k: None,
            video_queue: Vec::with_capacity(120),
            audio_queue: Vec::with_capacity(120),
            frag_entries: Vec::new(),
            bytes_written: 0,
        };
        s.write_ftyp()?;
        Ok(s)
    }

    pub fn set_h264_params(&mut self, sps: Vec<u8>, pps: Vec<u8>) {
        self.sps = Some(sps);
        self.pps = Some(pps);
    }

    fn write_all_counted(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data)?;
        self.bytes_written += data.len() as u64;
        Ok(())
    }

    fn write_ftyp(&mut self) -> Result<()> {
        let mut c = BoxCursor::new();
        let ftyp = c.begin(b"ftyp");
        c.push_bytes(b"isom");
        c.push_u32(512);
        c.push_bytes(b"isom");
        c.push_bytes(b"iso5");
        c.push_bytes(b"mp42");
        c.push_bytes(match self.video_codec {
            VideoCodec::H265 => b"hev1",
            VideoCodec::Av1 => b"av01",
            _ => b"avc1",
        });
        c.end(ftyp);
        self.write_all_counted(&c.buf)
    }

    fn write_moov_if_ready(&mut self) -> Result<()> {
        if self.moov_written {
            return Ok(());
        }
        let sps = self
            .sps
            .as_deref()
            .ok_or_else(|| VexrecError::Muxer("Missing H.264 SPS before moov emit".into()))?;
        let pps = self
            .pps
            .as_deref()
            .ok_or_else(|| VexrecError::Muxer("Missing H.264 PPS before moov emit".into()))?;
        let avcc = build_avcc_record(sps, pps);

        let mut c = BoxCursor::new();
        let moov = c.begin(b"moov");

        // mvhd
        let mvhd = c.begin_full(b"mvhd", 0, 0);
        c.push_u32(0); // creation_time
        c.push_u32(0); // mod_time
        c.push_u32(1000); // timescale
        c.push_u32(0); // duration (fragmented)
        c.push_u32(0x0001_0000); // rate 1.0
        c.push_u16(0x0100); // volume 1.0
        c.push_zeros(10);
        write_identity_matrix(&mut c);
        c.push_zeros(24);
        c.push_u32(if self.audio_enabled { 3 } else { 2 });
        c.end(mvhd);

        // Video Track (ID = 1)
        {
            let trak = c.begin(b"trak");
            let tkhd = c.begin_full(b"tkhd", 0, 0x0000_0003); // enabled + in_movie
            c.push_u32(0);
            c.push_u32(0);
            c.push_u32(1);
            c.push_u32(0);
            c.push_u32(0);
            c.push_zeros(8);
            c.push_u16(0);
            c.push_u16(0);
            c.push_u16(0);
            c.push_u16(0);
            write_identity_matrix(&mut c);
            c.push_u32(self.width << 16);
            c.push_u32(self.height << 16);
            c.end(tkhd);

            let mdia = c.begin(b"mdia");
            let mdhd = c.begin_full(b"mdhd", 0, 0);
            c.push_u32(0);
            c.push_u32(0);
            c.push_u32(VIDEO_TIMESCALE as u32);
            c.push_u32(0);
            c.push_u16(0x55c4); // 'und'
            c.push_u16(0);
            c.end(mdhd);

            let hdlr = c.begin_full(b"hdlr", 0, 0);
            c.push_u32(0);
            c.push_bytes(b"vide");
            c.push_zeros(12);
            c.push_bytes(b"Vexrec Video\0");
            c.end(hdlr);

            let minf = c.begin(b"minf");
            let vmhd = c.begin_full(b"vmhd", 0, 1);
            c.push_zeros(8);
            c.end(vmhd);

            write_dinf_local(&mut c);

            let stbl = c.begin(b"stbl");
            let stsd = c.begin_full(b"stsd", 0, 0);
            c.push_u32(1); // entry_count
            let avc1 = c.begin(b"avc1");
            c.push_zeros(6);
            c.push_u16(1); // data_reference_index
            c.push_zeros(16);
            c.push_u16(self.width as u16);
            c.push_u16(self.height as u16);
            c.push_u32(0x0048_0000); // 72 dpi
            c.push_u32(0x0048_0000);
            c.push_u32(0);
            c.push_u16(1); // frame_count
            c.push_zeros(32); // compressorname
            c.push_u16(0x0018); // depth 24
            c.push_i16(-1);

            let avcc_box = c.begin(b"avcC");
            c.push_bytes(&avcc);
            c.end(avcc_box);

            // Explicit BT.709 color descriptor box (nclx: primaries=1, transfer=1, matrix=1, full_range=0)
            let colr = c.begin(b"colr");
            c.push_bytes(b"nclx");
            c.push_u16(1);
            c.push_u16(1);
            c.push_u16(1);
            c.push_u8(0);
            c.end(colr);

            c.end(avc1);
            c.end(stsd);

            for empty in [b"stts", b"stsc", b"stco"] {
                let b = c.begin_full(empty, 0, 0);
                c.push_u32(0);
                c.end(b);
            }
            let stsz = c.begin_full(b"stsz", 0, 0);
            c.push_u32(0);
            c.push_u32(0);
            c.end(stsz);

            c.end(stbl);
            c.end(minf);
            c.end(mdia);
            c.end(trak);
        }

        // Audio Track (ID = 2) if enabled
        if self.audio_enabled {
            let trak = c.begin(b"trak");
            let tkhd = c.begin_full(b"tkhd", 0, 0x0000_0003);
            c.push_u32(0);
            c.push_u32(0);
            c.push_u32(2);
            c.push_u32(0);
            c.push_u32(0);
            c.push_zeros(8);
            c.push_u16(0);
            c.push_u16(0);
            c.push_u16(0x0100); // Audio volume 1.0
            c.push_u16(0);
            write_identity_matrix(&mut c);
            c.push_u32(0);
            c.push_u32(0);
            c.end(tkhd);

            let mdia = c.begin(b"mdia");
            let mdhd = c.begin_full(b"mdhd", 0, 0);
            c.push_u32(0);
            c.push_u32(0);
            c.push_u32(AUDIO_TIMESCALE as u32);
            c.push_u32(0);
            c.push_u16(0x55c4);
            c.push_u16(0);
            c.end(mdhd);

            let hdlr = c.begin_full(b"hdlr", 0, 0);
            c.push_u32(0);
            c.push_bytes(b"soun");
            c.push_zeros(12);
            c.push_bytes(b"Vexrec Audio\0");
            c.end(hdlr);

            let minf = c.begin(b"minf");
            let smhd = c.begin_full(b"smhd", 0, 0);
            c.push_u32(0);
            c.end(smhd);
            write_dinf_local(&mut c);

            let stbl = c.begin(b"stbl");
            let stsd = c.begin_full(b"stsd", 0, 0);
            c.push_u32(1);
            let opus = c.begin(b"Opus");
            c.push_zeros(6);
            c.push_u16(1);
            c.push_zeros(8);
            c.push_u16(self.audio_channels as u16);
            c.push_u16(16);
            c.push_zeros(4);
            c.push_u32((AUDIO_TIMESCALE as u32) << 16);

            let dops = c.begin(b"dOps");
            c.push_bytes(&build_opus_dops_mp4(self.audio_channels, self.opus_pre_skip, AUDIO_TIMESCALE as u32));
            c.end(dops);
            c.end(opus);
            c.end(stsd);

            for empty in [b"stts", b"stsc", b"stco"] {
                let b = c.begin_full(empty, 0, 0);
                c.push_u32(0);
                c.end(b);
            }
            let stsz = c.begin_full(b"stsz", 0, 0);
            c.push_u32(0);
            c.push_u32(0);
            c.end(stsz);

            c.end(stbl);
            c.end(minf);
            c.end(mdia);
            c.end(trak);
        }

        // mvex
        let mvex = c.begin(b"mvex");
        for track_id in if self.audio_enabled { [1u32, 2].as_slice() } else { [1u32].as_slice() } {
            let trex = c.begin_full(b"trex", 0, 0);
            c.push_u32(*track_id);
            c.push_u32(1); // default_sample_description_index
            c.push_u32(0);
            c.push_u32(0);
            c.push_u32(0);
            c.end(trex);
        }
        c.end(mvex);

        c.end(moov);
        self.write_all_counted(&c.buf)?;
        self.moov_written = true;
        Ok(())
    }

    pub fn push_video_frame(&mut self, pts_ns: u64, force_keyframe_hint: bool, annexb: &[u8]) -> Result<()> {
        if !self.moov_written && (self.sps.is_none() || self.pps.is_none()) {
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
        let is_key = is_idr || force_keyframe_hint;
        let mut pts_90k = (pts_ns as u128 * VIDEO_TIMESCALE as u128 / 1_000_000_000) as u64;
        if let Some(prev) = self.last_video_pts_90k {
            if pts_90k <= prev {
                pts_90k = prev + 1;
            }
            if let Some(last_sample) = self.video_queue.last_mut() {
                last_sample.duration_90k = (pts_90k - prev).max(1) as u32;
            }
        }
        let default_dur = (VIDEO_TIMESCALE / self.fps as u64).max(1) as u32;

        // Flush previous fragment cleanly after patching its final sample duration up to this keyframe's PTS
        if is_key && self.video_queue.len() >= self.fps as usize && self.sps.is_some() && self.pps.is_some() {
            self.flush_fragment()?;
        }

        self.last_video_pts_90k = Some(pts_90k);

        self.video_queue.push(QueuedVideoSample {
            pts_90k,
            duration_90k: default_dur,
            is_keyframe: is_key,
            payload: avcc_sample,
        });
        Ok(())
    }

    pub fn push_audio_packet(&mut self, pts_ns: u64, opus_pkt: &[u8]) -> Result<()> {
        if !self.audio_enabled || opus_pkt.is_empty() {
            return Ok(());
        }
        let mut pts_48k = (pts_ns as u128 * AUDIO_TIMESCALE as u128 / 1_000_000_000) as u64;
        if let Some(prev) = self.last_audio_pts_48k {
            if pts_48k <= prev {
                pts_48k = prev + 1;
            }
            if let Some(last) = self.audio_queue.last_mut() {
                last.duration_48k = (pts_48k - prev).clamp(120, 48_000) as u32;
            }
        }
        self.last_audio_pts_48k = Some(pts_48k);
        self.audio_queue.push(QueuedAudioSample {
            pts_48k,
            duration_48k: 960, // 20 ms default at 48 kHz
            payload: opus_pkt.to_vec(),
        });
        Ok(())
    }

    fn flush_fragment(&mut self) -> Result<()> {
        if self.video_queue.is_empty() && self.audio_queue.is_empty() {
            return Ok(());
        }
        self.write_moov_if_ready()?;

        let moof_offset = self.bytes_written;
        if let Some(first_v) = self.video_queue.first() {
            self.frag_entries.push(FragIndexEntry {
                time_90k: first_v.pts_90k,
                moof_offset,
            });
        }

        let has_video = !self.video_queue.is_empty();
        let has_audio = self.audio_enabled && !self.audio_queue.is_empty();

        let mut moof_c = BoxCursor::new();
        let moof = moof_c.begin(b"moof");
        let mfhd = moof_c.begin_full(b"mfhd", 0, 0);
        moof_c.push_u32(self.seq_number);
        moof_c.end(mfhd);
        self.seq_number += 1;

        let mut video_data_offset_pos: Option<usize> = None;
        let mut audio_data_offset_pos: Option<usize> = None;

        let video_bytes_len: usize = self.video_queue.iter().map(|s| s.payload.len()).sum();
        let audio_bytes_len: usize = self.audio_queue.iter().map(|s| s.payload.len()).sum();

        if has_video {
            let traf = moof_c.begin(b"traf");
            let tfhd = moof_c.begin_full(b"tfhd", 0, 0x0002_0000); // default-base-is-moof
            moof_c.push_u32(1);
            moof_c.end(tfhd);

            let base_time = self.video_queue.first().map(|s| s.pts_90k).unwrap_or(self.video_base_decode_90k);
            let tfdt = moof_c.begin_full(b"tfdt", 1, 0);
            moof_c.push_u64(base_time);
            moof_c.end(tfdt);

            // flags: data-offset-present (0x0001) + duration (0x0100) + size (0x0200) + flags (0x0400) = 0x0701
            let trun = moof_c.begin_full(b"trun", 0, 0x0007_01);
            moof_c.push_u32(self.video_queue.len() as u32);
            video_data_offset_pos = Some(moof_c.buf.len());
            moof_c.push_i32(0); // patched once moof size is known

            for s in &self.video_queue {
                moof_c.push_u32(s.duration_90k);
                moof_c.push_u32(s.payload.len() as u32);
                let sflags: u32 = if s.is_keyframe { 0x0200_0000 } else { 0x0101_0000 };
                moof_c.push_u32(sflags);
                self.video_base_decode_90k = s.pts_90k + s.duration_90k as u64;
            }
            moof_c.end(trun);
            moof_c.end(traf);
        }

        if has_audio {
            let traf = moof_c.begin(b"traf");
            let tfhd = moof_c.begin_full(b"tfhd", 0, 0x0002_0000);
            moof_c.push_u32(2);
            moof_c.end(tfhd);

            let base_time = self.audio_queue.first().map(|s| s.pts_48k).unwrap_or(self.audio_base_decode_48k);
            let tfdt = moof_c.begin_full(b"tfdt", 1, 0);
            moof_c.push_u64(base_time);
            moof_c.end(tfdt);

            let trun = moof_c.begin_full(b"trun", 0, 0x0003_01); // data-offset + duration + size
            moof_c.push_u32(self.audio_queue.len() as u32);
            audio_data_offset_pos = Some(moof_c.buf.len());
            moof_c.push_i32(0);

            for s in &self.audio_queue {
                moof_c.push_u32(s.duration_48k);
                moof_c.push_u32(s.payload.len() as u32);
                self.audio_base_decode_48k = s.pts_48k + s.duration_48k as u64;
            }
            moof_c.end(trun);
            moof_c.end(traf);
        }

        moof_c.end(moof);
        let moof_len = moof_c.buf.len();
        if let Some(pos) = video_data_offset_pos {
            let offset = (moof_len + 8) as i32;
            moof_c.buf[pos..pos + 4].copy_from_slice(&offset.to_be_bytes());
        }
        if let Some(pos) = audio_data_offset_pos {
            let offset = (moof_len + 8 + video_bytes_len) as i32;
            moof_c.buf[pos..pos + 4].copy_from_slice(&offset.to_be_bytes());
        }

        self.write_all_counted(&moof_c.buf)?;

        // Write `mdat` header + sample payloads
        let mdat_size = (8 + video_bytes_len + audio_bytes_len) as u32;
        self.write_all_counted(&mdat_size.to_be_bytes())?;
        self.write_all_counted(b"mdat")?;

        let vq = std::mem::take(&mut self.video_queue);
        for s in vq {
            self.write_all_counted(&s.payload)?;
        }
        let aq = std::mem::take(&mut self.audio_queue);
        for s in aq {
            self.write_all_counted(&s.payload)?;
        }

        self.writer.flush()?;
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        if !self.video_queue.is_empty() || !self.audio_queue.is_empty() {
            if self.sps.is_some() && self.pps.is_some() {
                self.flush_fragment()?;
            }
        }
        if self.moov_written && !self.frag_entries.is_empty() {
            let mut c = BoxCursor::new();
            let mfra = c.begin(b"mfra");
            let tfra = c.begin_full(b"tfra", 1, 0);
            c.push_u32(1); // track_id
            c.push_u32(0); // length sizes (0 = 1 byte each for traf_num, trun_num, sample_num)
            c.push_u32(self.frag_entries.len() as u32);
            for entry in &self.frag_entries {
                c.push_u64(entry.time_90k);
                c.push_u64(entry.moof_offset);
                c.push_u8(1);
                c.push_u8(1);
                c.push_u8(1);
            }
            c.end(tfra);

            let mfro_start = c.buf.len();
            let mfro = c.begin_full(b"mfro", 0, 0);
            let total_mfra = (mfro_start - mfra + 16) as u32;
            c.push_u32(total_mfra);
            c.end(mfro);
            c.end(mfra);
            self.write_all_counted(&c.buf)?;
        }
        self.writer.flush()?;
        let _ = self.writer.get_mut().seek(SeekFrom::End(0));
        Ok(())
    }
}

fn write_dinf_local(c: &mut BoxCursor) {
    let dinf = c.begin(b"dinf");
    let dref = c.begin_full(b"dref", 0, 0);
    c.push_u32(1);
    let url = c.begin_full(b"url ", 0, 1); // self-contained
    c.end(url);
    c.end(dref);
    c.end(dinf);
}

fn write_identity_matrix(c: &mut BoxCursor) {
    c.push_u32(0x0001_0000);
    c.push_u32(0);
    c.push_u32(0);
    c.push_u32(0);
    c.push_u32(0x0001_0000);
    c.push_u32(0);
    c.push_u32(0);
    c.push_u32(0);
    c.push_u32(0x4000_0000);
}
