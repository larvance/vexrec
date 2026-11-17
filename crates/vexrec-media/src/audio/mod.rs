pub mod alsa;
pub mod pulse_simple;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use nnnoiseless::DenoiseState;
use opus_pure::{Application, MAX_PACKET_BYTES, OpusEncoder, OpusHead};
use tracing::{info, warn};
use vexrec_core::config::AudioConfig;
use vexrec_core::error::{Result, VexrecError};

use alsa::{AlsaCaptureReader, run_alsa_capture_loop};
use pulse_simple::{PulseSimpleReader, run_pulse_capture_loop};

pub const OPUS_SAMPLE_RATE: u32 = 48_000;
pub const OPUS_FRAME_SAMPLES: usize = 960; // 20 ms @ 48 kHz
pub const OPUS_CHANNELS: usize = 2;

pub struct EncodedAudioPacket {
    pub pts_ns: u64,
    pub data: Vec<u8>,
}

/// Real-time lock-free audio capture, neural RNNoise denoiser (`nnnoiseless`),
/// dual-bus mixer, and pure-Rust RFC 6716 `opus-pure` encoder engine.
pub struct AudioMixerEngine {
    stop: Arc<AtomicBool>,
    mic_rms: Arc<AtomicU32>,
    desk_rms: Arc<AtomicU32>,
    pre_skip: u16,
    threads: Vec<JoinHandle<()>>,
}

impl AudioMixerEngine {
    pub fn start(
        cfg: AudioConfig,
        record_mic: bool,
        record_desktop: bool,
        paused: Arc<AtomicBool>,
        start_mono_ns: u64,
        paused_total_ns: Arc<AtomicU64>,
        tx: SyncSender<EncodedAudioPacket>,
    ) -> Result<Self> {
        let mut opus = OpusEncoder::new(OPUS_SAMPLE_RATE as i32, OPUS_CHANNELS, Application::Audio)
            .map_err(|e| VexrecError::Audio(format!("OpusEncoder init failed: {e}")))?;
        opus.bitrate_bps = 128_000;
        let head = OpusHead::for_encoder(&opus, OPUS_SAMPLE_RATE);
        let pre_skip = head.pre_skip;

        let stop = Arc::new(AtomicBool::new(false));
        let mic_rms = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let desk_rms = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let mut threads = Vec::new();

        let (desk_tx, mut desk_rx) = rtrb::RingBuffer::<f32>::new(OPUS_SAMPLE_RATE as usize * 4);
        let (mic_tx, mut mic_rx) = rtrb::RingBuffer::<f32>::new(OPUS_SAMPLE_RATE as usize * 2);

        let mut active_desktop = false;
        if record_desktop {
            let dev_name = match cfg.desktop_device.as_deref() {
                Some(d) if !d.is_empty() && d != "default" => d,
                _ => "@DEFAULT_SINK@.monitor",
            };
            match PulseSimpleReader::open(Some(dev_name), "Vexrec Desktop Monitor", OPUS_SAMPLE_RATE, 2) {
                Ok(reader) => {
                    active_desktop = true;
                    let stop_c = Arc::clone(&stop);
                    if let Ok(h) = thread::Builder::new()
                        .name("vexrec-aud-desk".into())
                        .spawn(move || run_pulse_capture_loop(reader, desk_tx, &stop_c))
                    {
                        threads.push(h);
                    }
                    info!(device = dev_name, "Desktop monitor audio capture started");
                }
                Err(e) => warn!("Desktop audio monitor unavailable ({e}); continuing gracefully"),
            }
        }

        let mut active_mic = false;
        if record_mic {
            let dev_opt = match cfg.microphone_device.as_deref() {
                Some(d) if !d.is_empty() && d != "default" => Some(d),
                _ => Some("@DEFAULT_SOURCE@"),
            };
            let pulse_res = PulseSimpleReader::open(dev_opt, "Vexrec Microphone", OPUS_SAMPLE_RATE, 1)
                .or_else(|_| PulseSimpleReader::open(None, "Vexrec Microphone", OPUS_SAMPLE_RATE, 1));
            match pulse_res {
                Ok(reader) => {
                    active_mic = true;
                    let stop_c = Arc::clone(&stop);
                    if let Ok(h) = thread::Builder::new()
                        .name("vexrec-aud-mic".into())
                        .spawn(move || run_pulse_capture_loop(reader, mic_tx, &stop_c))
                    {
                        threads.push(h);
                    }
                    info!("Microphone audio capture started via Pulse/PipeWire");
                }
                Err(pe) => match AlsaCaptureReader::open_default(OPUS_SAMPLE_RATE, 1) {
                    Ok(alsa_reader) => {
                        active_mic = true;
                        let stop_c = Arc::clone(&stop);
                        if let Ok(h) = thread::Builder::new()
                            .name("vexrec-aud-mic-alsa".into())
                            .spawn(move || run_alsa_capture_loop(alsa_reader, mic_tx, &stop_c))
                        {
                            threads.push(h);
                        }
                        info!("Microphone audio capture started via ALSA fallback");
                    }
                    Err(ae) => {
                        warn!("Microphone audio unavailable (Pulse: {pe}; ALSA: {ae}); continuing without mic")
                    }
                },
            }
        }

        let stop_mix = Arc::clone(&stop);
        let mic_rms_c = Arc::clone(&mic_rms);
        let desk_rms_c = Arc::clone(&desk_rms);
        let mix_handle = thread::Builder::new()
            .name("vexrec-aud-mix".into())
            .spawn(move || {
                let mut denoiser = if cfg.noise_suppression && active_mic {
                    let mut d = DenoiseState::new();
                    let warmup = [0.0f32; DenoiseState::FRAME_SIZE];
                    let mut sink = [0.0f32; DenoiseState::FRAME_SIZE];
                    d.process_frame(&mut sink, &warmup);
                    Some(d)
                } else {
                    None
                };

                let mut packet_buf = vec![0u8; MAX_PACKET_BYTES];
                let mut stereo_mix = [0.0f32; OPUS_FRAME_SAMPLES * OPUS_CHANNELS];
                let mut desk_buf = [0.0f32; OPUS_FRAME_SAMPLES * OPUS_CHANNELS];
                let mut mic_buf = [0.0f32; OPUS_FRAME_SAMPLES];
                let mut denoise_in = [0.0f32; DenoiseState::FRAME_SIZE];
                let mut denoise_out = [0.0f32; DenoiseState::FRAME_SIZE];

                let desk_gain = cfg.desktop_volume.clamp(0.0, 4.0);
                let mic_gain = cfg.microphone_volume.clamp(0.0, 4.0);
                let mut frame_index: u64 = 0;

                while !stop_mix.load(Ordering::Relaxed) {
                    if paused.load(Ordering::Relaxed) {
                        // Drain queued buffers while paused so old audio isn't emitted on resume
                        let d_avail = desk_rx.slots();
                        if d_avail > 0 {
                            if let Ok(chunk) = desk_rx.read_chunk(d_avail) {
                                chunk.commit_all();
                            }
                        }
                        let m_avail = mic_rx.slots();
                        if m_avail > 0 {
                            if let Ok(chunk) = mic_rx.read_chunk(m_avail) {
                                chunk.commit_all();
                            }
                        }
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }

                    // Paced directly against CLOCK_MONOTONIC minus paused duration so silence in monitor never drifts A/V sync
                    let now_ns = monotonic_now_ns();
                    let target_elapsed_ns = (frame_index + 1) * 20_000_000; // 20 ms steps
                    let p_ns = paused_total_ns.load(Ordering::Relaxed);
                    let elapsed_ns = now_ns.saturating_sub(start_mono_ns).saturating_sub(p_ns);
                    if elapsed_ns + 1_000_000 < target_elapsed_ns {
                        let wait_ns = target_elapsed_ns - elapsed_ns;
                        thread::sleep(Duration::from_nanos(wait_ns.min(20_000_000)));
                        continue;
                    }

                    // Pull 20 ms stereo desktop samples if available, else clean silence
                    desk_buf.fill(0.0);
                    if active_desktop {
                        let avail = desk_rx.slots();
                        if avail >= desk_buf.len() {
                            let _ = desk_rx.pop_entire_slice(&mut desk_buf);
                        } else if avail > 0 {
                            let _ = desk_rx.pop_partial_slice(&mut desk_buf[..avail]);
                        }
                    }

                    // Pull 20 ms mono mic samples (2 x 480-sample RNNoise blocks) if available
                    mic_buf.fill(0.0);
                    if active_mic {
                        let avail = mic_rx.slots();
                        if avail >= mic_buf.len() {
                            let _ = mic_rx.pop_entire_slice(&mut mic_buf);
                        } else if avail > 0 {
                            let _ = mic_rx.pop_partial_slice(&mut mic_buf[..avail]);
                        }
                    }

                    if let Some(ref mut dn) = denoiser {
                        // nnnoiseless explicitly expects f32 scaled to 16-bit integer range [-32768.0, 32767.0]
                        for half in 0..2 {
                            let off = half * DenoiseState::FRAME_SIZE;
                            for i in 0..DenoiseState::FRAME_SIZE {
                                denoise_in[i] = mic_buf[off + i] * 32767.0;
                            }
                            dn.process_frame(&mut denoise_out, &denoise_in);
                            for i in 0..DenoiseState::FRAME_SIZE {
                                mic_buf[off + i] = denoise_out[i] / 32767.0;
                            }
                        }
                    }

                    // Dual-bus mix into stereo interleaved + compute instantaneous RMS levels
                    let mut desk_sq = 0.0f32;
                    let mut mic_sq = 0.0f32;
                    for i in 0..OPUS_FRAME_SAMPLES {
                        let dl = desk_buf[i * 2] * desk_gain;
                        let dr = desk_buf[i * 2 + 1] * desk_gain;
                        let m = mic_buf[i] * mic_gain;
                        desk_sq += (dl * dl + dr * dr) * 0.5;
                        mic_sq += m * m;
                        stereo_mix[i * 2] = soft_limit(dl + m);
                        stereo_mix[i * 2 + 1] = soft_limit(dr + m);
                    }
                    let d_rms = (desk_sq / OPUS_FRAME_SAMPLES as f32).sqrt();
                    let m_rms = (mic_sq / OPUS_FRAME_SAMPLES as f32).sqrt();
                    desk_rms_c.store(d_rms.to_bits(), Ordering::Relaxed);
                    mic_rms_c.store(m_rms.to_bits(), Ordering::Relaxed);

                    if let Ok(len) = opus.encode(&stereo_mix, OPUS_FRAME_SAMPLES, &mut packet_buf) {
                        let pts_ns = frame_index * 20_000_000;
                        let _ = tx.send(EncodedAudioPacket {
                            pts_ns,
                            data: packet_buf[..len].to_vec(),
                        });
                    }
                    frame_index += 1;
                }
            })
            .map_err(|e| VexrecError::Audio(format!("Failed to spawn audio mixer thread: {e}")))?;

        threads.push(mix_handle);
        Ok(Self {
            stop,
            mic_rms,
            desk_rms,
            pre_skip,
            threads,
        })
    }

    pub fn pre_skip(&self) -> u16 {
        self.pre_skip
    }

    pub fn mic_rms(&self) -> f32 {
        f32::from_bits(self.mic_rms.load(Ordering::Relaxed))
    }

    pub fn desktop_rms(&self) -> f32 {
        f32::from_bits(self.desk_rms.load(Ordering::Relaxed))
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for h in self.threads.drain(..) {
            let _ = h.join();
        }
    }
}

impl Drop for AudioMixerEngine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for h in self.threads.drain(..) {
            let _ = h.join();
        }
    }
}

#[inline(always)]
fn soft_limit(x: f32) -> f32 {
    if x.abs() <= 0.92 {
        x
    } else {
        x.signum() * (0.92 + 0.08 * ((x.abs() - 0.92) / 0.08).tanh())
    }
}

#[inline]
pub fn monotonic_now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}
