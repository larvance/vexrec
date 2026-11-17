use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use gif::{Encoder as GifEncoder, Frame as GifFrame, Repeat};
use tracing::{error, info, warn};
use vexrec_core::buffer::{FrameBacking, PixelFormat, VideoFrame};
use vexrec_core::config::VexrecConfig;
use vexrec_core::error::{Result, VexrecError};
use vexrec_core::formats::{ContainerFormat, VideoCodec};
use vexrec_core::geometry::Rect;
use vexrec_os::simd::bgrx_to_rgba;

use crate::audio::{monotonic_now_ns, AudioMixerEngine, EncodedAudioPacket};
use crate::capture::VideoCaptureSource;
use crate::encoder::sw::OpenH264Encoder;
use crate::encoder::{create_best_encoder, VideoEncoder};
use crate::mux::MediaMuxer;

#[derive(Debug, Clone)]
pub struct RecordingSummary {
    pub path: PathBuf,
    pub duration_secs: u64,
    pub frames: u64,
    pub encoder_backend: String,
    pub bytes_written: u64,
}

struct GifRecordingSink {
    enc: GifEncoder<BufWriter<File>>,
    width: u16,
    height: u16,
    delay_cs: u16,
    rgba_buf: Vec<u8>,
}

impl GifRecordingSink {
    fn create(path: &Path, width: u32, height: u32, fps: u32) -> Result<Self> {
        let w = width.clamp(16, u16::MAX as u32) as u16;
        let h = height.clamp(16, u16::MAX as u32) as u16;
        let eff_fps = fps.clamp(1, 25);
        let delay_cs = (100 / eff_fps).max(4) as u16;
        let file = File::create(path)?;
        let bw = BufWriter::with_capacity(512 * 1024, file);
        let mut enc = GifEncoder::new(bw, w, h, &[])
            .map_err(|e| VexrecError::Encoding(format!("GIF encoder init failed: {e}")))?;
        enc.set_repeat(Repeat::Infinite)
            .map_err(|e| VexrecError::Encoding(format!("GIF repeat failed: {e}")))?;
        Ok(Self {
            enc,
            width: w,
            height: h,
            delay_cs,
            rgba_buf: vec![0u8; (w as usize) * (h as usize) * 4],
        })
    }

    fn push_frame(&mut self, frame: &VideoFrame) -> Result<()> {
        let FrameBacking::Host { stride, ref data } = frame.backing else {
            return Err(VexrecError::Encoding("GIF sink requires host buffer".into()));
        };
        let src = data.as_slice();
        let w = self.width as usize;
        let h = self.height as usize;
        let row_bytes = w * 4;
        for row in 0..h {
            let s_off = row * (stride as usize);
            let d_off = row * row_bytes;
            if s_off + row_bytes <= src.len() {
                if frame.format == PixelFormat::Rgba8 {
                    self.rgba_buf[d_off..d_off + row_bytes].copy_from_slice(&src[s_off..s_off + row_bytes]);
                } else {
                    bgrx_to_rgba(&src[s_off..s_off + row_bytes], &mut self.rgba_buf[d_off..d_off + row_bytes]);
                }
            }
        }
        let mut gf = GifFrame::from_rgba_speed(self.width, self.height, &mut self.rgba_buf, 10);
        gf.delay = self.delay_cs;
        self.enc
            .write_frame(&gf)
            .map_err(|e| VexrecError::Encoding(format!("GIF frame write error: {e}")))
    }
}

/// Real-time OS screen + dual-bus audio recorder orchestrating:
/// Capture thread -> bounded SPSC frame channel -> HW/SW Encoder thread + Opus AudioMixer -> fMP4/MKV/WebM/GIF.
pub struct RecordingPipeline {
    output_path: PathBuf,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    pause_started_ns: Arc<AtomicU64>,
    paused_total_ns: Arc<AtomicU64>,
    start_mono_ns: u64,
    frames_written: Arc<AtomicU64>,
    encoder_name: Arc<std::sync::Mutex<String>>,
    audio_engine: Option<AudioMixerEngine>,
    capture_thread: Option<JoinHandle<()>>,
    mux_thread: Option<JoinHandle<Result<()>>>,
}

impl RecordingPipeline {
    pub fn start(
        mut cfg: VexrecConfig,
        output_path: PathBuf,
        crop: Option<Rect>,
        record_mic: bool,
        record_desktop: bool,
    ) -> Result<Self> {
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let is_gif = cfg.recording.container == ContainerFormat::Gif;
        let req_fps = if is_gif {
            cfg.recording.framerate.clamp(1, 25)
        } else {
            cfg.recording.framerate.clamp(1, 120)
        };

        // Ensure container compatibility for MP4/MKV/WebM with H.264 bitstream framing
        if !is_gif && cfg.recording.video_codec == VideoCodec::Av1 {
            warn!("Muxing H.264 into container instead of AV1 for universal MP4/MKV player compatibility");
            cfg.recording.video_codec = VideoCodec::H264;
        }

        let mut source = VideoCaptureSource::open_auto(crop, req_fps, cfg.recording.show_cursor)?;
        let width = source.width();
        let height = source.height();
        let src_name = source.backend_name();

        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let pause_started_ns = Arc::new(AtomicU64::new(0));
        let paused_total_ns = Arc::new(AtomicU64::new(0));
        let frames_written = Arc::new(AtomicU64::new(0));
        let start_mono_ns = monotonic_now_ns();

        let audio_enabled = !is_gif && (record_mic || record_desktop);
        let (aud_tx, aud_rx): (Option<SyncSender<EncodedAudioPacket>>, Option<Receiver<EncodedAudioPacket>>) =
            if audio_enabled {
                let (tx, rx) = mpsc::sync_channel(256);
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };

        let audio_engine = if let Some(tx) = aud_tx {
            match AudioMixerEngine::start(
                cfg.audio.clone(),
                record_mic,
                record_desktop,
                Arc::clone(&paused),
                start_mono_ns,
                Arc::clone(&paused_total_ns),
                tx,
            ) {
                Ok(eng) => Some(eng),
                Err(e) => {
                    warn!("Audio engine failed to start ({e}); recording video-only");
                    None
                }
            }
        } else {
            None
        };

        let pre_skip = audio_engine.as_ref().map_or(312, |e| e.pre_skip());
        let effective_audio = audio_engine.is_some();

        let encoder_name = Arc::new(std::sync::Mutex::new(if is_gif {
            "Pure Rust GIF".to_string()
        } else {
            "Auto".to_string()
        }));

        let (vid_tx, vid_rx) = mpsc::sync_channel::<VideoFrame>(4);

        let mux_path = output_path.clone();
        let rec_cfg = cfg.recording.clone();
        let frames_c = Arc::clone(&frames_written);
        let enc_name_c = Arc::clone(&encoder_name);

        let mux_thread = thread::Builder::new()
            .name("vexrec-rec-mux".into())
            .spawn(move || -> Result<()> {
                if is_gif {
                    let mut sink = GifRecordingSink::create(&mux_path, width, height, req_fps)?;
                    for frame in vid_rx {
                        if let Err(e) = sink.push_frame(&frame) {
                            error!("GIF frame error: {e}");
                            break;
                        }
                        frames_c.fetch_add(1, Ordering::Relaxed);
                    }
                    return Ok(());
                }

                let mut encoder: Box<dyn VideoEncoder> = create_best_encoder(width, height, &rec_cfg)?;
                if let Ok(mut g) = enc_name_c.lock() {
                    *g = encoder.backend_name().to_string();
                }

                let mut muxer = MediaMuxer::create(
                    &mux_path,
                    rec_cfg.container,
                    width,
                    height,
                    req_fps,
                    VideoCodec::H264,
                    2,
                    effective_audio,
                    pre_skip,
                )?;
                if let Some((sps, pps)) = encoder.sps_pps() {
                    muxer.set_h264_params(sps.to_vec(), pps.to_vec());
                }

                let mut first_video_encoded = false;
                let drain_audio = |muxer: &mut MediaMuxer| -> Result<()> {
                    if let Some(ref rx) = aud_rx {
                        loop {
                            match rx.try_recv() {
                                Ok(pkt) => {
                                    muxer.push_audio_packet(pkt.pts_ns, &pkt.data)?;
                                }
                                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                            }
                        }
                    }
                    Ok(())
                };

                while let Ok(frame) = vid_rx.recv() {
                    let _ = drain_audio(&mut muxer);

                    let force_key = !first_video_encoded;
                    let pkts = match encoder.encode(&frame, force_key) {
                        Ok(p) => p,
                        Err(e) if !first_video_encoded => {
                            warn!(backend = encoder.backend_name(), "HW encoder failed on frame 0 ({e}), switching seamlessly to CPU OpenH264");
                            let sw = OpenH264Encoder::new(width, height, req_fps, rec_cfg.quality)?;
                            if let Ok(mut g) = enc_name_c.lock() {
                                *g = sw.backend_name().to_string();
                            }
                            encoder = Box::new(sw);
                            encoder.encode(&frame, true)?
                        }
                        Err(e) => {
                            error!("Video encode error: {e}");
                            break;
                        }
                    };

                    if let Some((sps, pps)) = encoder.sps_pps() {
                        muxer.set_h264_params(sps.to_vec(), pps.to_vec());
                    }

                    for p in pkts {
                        muxer.push_video_frame(p.pts_ns, p.is_keyframe, &p.data)?;
                        first_video_encoded = true;
                        frames_c.fetch_add(1, Ordering::Relaxed);
                    }
                }

                // Flush encoder tail and residual audio
                if let Ok(tail) = encoder.flush() {
                    for p in tail {
                        let _ = muxer.push_video_frame(p.pts_ns, p.is_keyframe, &p.data);
                        frames_c.fetch_add(1, Ordering::Relaxed);
                    }
                }
                let _ = drain_audio(&mut muxer);
                muxer.finish()?;
                Ok(())
            })
            .map_err(|e| VexrecError::Encoding(format!("Failed to spawn mux thread: {e}")))?;

        let stop_cap = Arc::clone(&stop);
        let paused_cap = Arc::clone(&paused);
        let paused_total_cap = Arc::clone(&paused_total_ns);
        let frame_interval_ns = 1_000_000_000u64 / req_fps as u64;

        let capture_thread = thread::Builder::new()
            .name("vexrec-rec-cap".into())
            .spawn(move || {
                info!(capture = src_name, width, height, fps = req_fps, "Recording capture loop started");
                let mut frame_idx: u64 = 0;

                while !stop_cap.load(Ordering::Relaxed) {
                    if paused_cap.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(12));
                        continue;
                    }

                    let target_elapsed_ns = frame_idx * frame_interval_ns;
                    let now_ns = monotonic_now_ns();
                    let p_ns = paused_total_cap.load(Ordering::Relaxed);
                    let elapsed_ns = now_ns.saturating_sub(start_mono_ns).saturating_sub(p_ns);

                    if elapsed_ns + 500_000 < target_elapsed_ns {
                        let wait = target_elapsed_ns - elapsed_ns;
                        thread::sleep(Duration::from_nanos(wait.min(frame_interval_ns)));
                        continue;
                    }

                    match source.capture_frame() {
                        Ok(mut vf) => {
                            vf.pts_ns = elapsed_ns;
                            match vid_tx.try_send(vf) {
                                Ok(()) => {}
                                Err(TrySendError::Full(_)) => {
                                    // Preserve strict pacing without building capture latency
                                }
                                Err(TrySendError::Disconnected(_)) => break,
                            }
                            frame_idx += 1;
                        }
                        Err(e) => {
                            warn!("Capture frame skipped: {e}");
                            thread::sleep(Duration::from_millis(4));
                        }
                    }
                }
            })
            .map_err(|e| VexrecError::Display(format!("Failed to spawn capture thread: {e}")))?;

        Ok(Self {
            output_path,
            stop,
            paused,
            pause_started_ns,
            paused_total_ns,
            start_mono_ns,
            frames_written,
            encoder_name,
            audio_engine,
            capture_thread: Some(capture_thread),
            mux_thread: Some(mux_thread),
        })
    }

    pub fn pause(&self) {
        if !self.paused.swap(true, Ordering::SeqCst) {
            self.pause_started_ns.store(monotonic_now_ns(), Ordering::SeqCst);
        }
    }

    pub fn resume(&self) {
        if self.paused.swap(false, Ordering::SeqCst) {
            let ps = self.pause_started_ns.swap(0, Ordering::SeqCst);
            if ps > 0 {
                let delta = monotonic_now_ns().saturating_sub(ps);
                self.paused_total_ns.fetch_add(delta, Ordering::SeqCst);
            }
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn elapsed_seconds(&self) -> u64 {
        let now = monotonic_now_ns();
        let active_pause = if self.is_paused() {
            let ps = self.pause_started_ns.load(Ordering::Relaxed);
            if ps > 0 {
                now.saturating_sub(ps)
            } else {
                0
            }
        } else {
            0
        };
        let total_pause = self.paused_total_ns.load(Ordering::Relaxed) + active_pause;
        now.saturating_sub(self.start_mono_ns)
            .saturating_sub(total_pause)
            / 1_000_000_000
    }

    pub fn frames_written(&self) -> u64 {
        self.frames_written.load(Ordering::Relaxed)
    }

    pub fn audio_levels(&self) -> (f32, f32) {
        match &self.audio_engine {
            Some(eng) => (eng.mic_rms(), eng.desktop_rms()),
            None => (0.0, 0.0),
        }
    }

    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    pub fn stop(mut self) -> Result<RecordingSummary> {
        self.stop.store(true, Ordering::SeqCst);
        let duration_secs = self.elapsed_seconds();

        if let Some(h) = self.capture_thread.take() {
            let _ = h.join();
        }
        if let Some(eng) = self.audio_engine.take() {
            eng.stop();
        }
        if let Some(h) = self.mux_thread.take() {
            match h.join() {
                Ok(res) => res?,
                Err(_) => return Err(VexrecError::Encoding("Mux thread panicked".into())),
            }
        }

        let frames = self.frames_written.load(Ordering::Relaxed);
        let encoder_backend = self
            .encoder_name
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "Unknown".into());
        let bytes_written = std::fs::metadata(&self.output_path)
            .map(|m| m.len())
            .unwrap_or(0);

        info!(
            path = %self.output_path.display(),
            duration_secs,
            frames,
            bytes_written,
            backend = %encoder_backend,
            "Recording finalized cleanly"
        );

        Ok(RecordingSummary {
            path: self.output_path.clone(),
            duration_secs,
            frames,
            encoder_backend,
            bytes_written,
        })
    }

    pub fn discard(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.capture_thread.take() {
            let _ = h.join();
        }
        if let Some(eng) = self.audio_engine.take() {
            eng.stop();
        }
        if let Some(h) = self.mux_thread.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_file(&self.output_path);
    }
}

/// Returns the active atomic meter handles cleanly if needed by settings/HUD.
pub fn rms_to_dbfs(rms: f32) -> f32 {
    if rms <= 1.0e-6 {
        -96.0
    } else {
        20.0 * rms.log10()
    }
}

#[allow(dead_code)]
fn _assert_atomic_u32(_: AtomicU32) {}
