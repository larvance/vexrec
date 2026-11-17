use std::fs;
use std::process::Command;
use opus_pure::{Application, MAX_PACKET_BYTES, OpusEncoder, OpusHead};
use vexrec_core::buffer::{AlignedVec, FrameBacking, PixelFormat, PixelImage, VideoFrame};
use vexrec_core::config::VexrecConfig;
use vexrec_core::formats::{CompressionSpeed, ContainerFormat, EncoderBackendPreference, ImageFormat, VideoCodec, VideoQuality};
use vexrec_core::geometry::Rect;
use vexrec_media::encoder::nvenc::probe_nvenc_runtime;
use vexrec_media::encoder::sw::{OpenH264Encoder, Rav1eAv1Encoder};
use vexrec_media::encoder::vaapi::probe_vaapi_h264;
use vexrec_media::encoder::vulkan_video::probe_vulkan_video;
use vexrec_media::encoder::VideoEncoder;
use vexrec_media::mux::MediaMuxer;
use vexrec_media::{save_snapshot, RecordingPipeline};

fn make_gradient_frame(w: u32, h: u32, idx: u32) -> VideoFrame {
    let stride = w * 4;
    let mut data = AlignedVec::zeroed((stride * h) as usize);
    let buf = data.as_mut_slice();
    for y in 0..h {
        for x in 0..w {
            let off = ((y * w + x) * 4) as usize;
            buf[off] = ((x * 2 + idx * 5) & 0xFF) as u8;     // B
            buf[off + 1] = ((y * 2 + idx * 3) & 0xFF) as u8; // G
            buf[off + 2] = (((x + y + idx * 7) & 0xFF)) as u8; // R
            buf[off + 3] = 255;
        }
    }
    VideoFrame {
        width: w,
        height: h,
        format: PixelFormat::Bgrx8,
        pts_ns: (idx as u64) * 33_333_333,
        damaged: true,
        backing: FrameBacking::Host { stride, data },
    }
}

#[test]
fn test_hardware_encoder_probes() {
    let vk = probe_vulkan_video();
    let va = probe_vaapi_h264();
    let nv = probe_nvenc_runtime();
    println!("VulkanVideo: {:?}", vk);
    println!("VA-API H264: {:?}", va);
    println!("NVENC runtime: {:?}", nv);
}

#[test]
fn test_h264_opus_fmp4_and_mkv_muxing() {
    let dir = std::env::temp_dir().join(format!("vexrec_media_test_{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);

    let (w, h, fps) = (128u32, 96u32, 30u32);
    let mp4_path = dir.join("out.mp4");
    let mkv_path = dir.join("out.mkv");

    let mut enc = OpenH264Encoder::new(w, h, fps, VideoQuality::Medium).unwrap();
    let mut opus = OpusEncoder::new(48_000, 2, Application::Audio).unwrap();
    opus.bitrate_bps = 96_000;
    let pre_skip = OpusHead::for_encoder(&opus, 48_000).pre_skip;

    let mut mp4 = MediaMuxer::create(&mp4_path, ContainerFormat::Mp4, w, h, fps, VideoCodec::H264, 2, true, pre_skip).unwrap();
    let mut mkv = MediaMuxer::create(&mkv_path, ContainerFormat::Mkv, w, h, fps, VideoCodec::H264, 2, true, pre_skip).unwrap();

    // Generate 45 audio 20ms packets (0.9s) and 27 video frames (0.9s)
    let mut pkt_buf = vec![0u8; MAX_PACKET_BYTES];
    let mut pcm = [0.0f32; 960 * 2];
    for i in 0..45u64 {
        for s in 0..960usize {
            let t = ((i as usize * 960 + s) as f32) / 48_000.0;
            let v = (t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.25;
            pcm[s * 2] = v;
            pcm[s * 2 + 1] = v;
        }
        let n = opus.encode(&pcm, 960, &mut pkt_buf).unwrap();
        let pts_ns = i * 20_000_000;
        mp4.push_audio_packet(pts_ns, &pkt_buf[..n]).unwrap();
        mkv.push_audio_packet(pts_ns, &pkt_buf[..n]).unwrap();
    }

    for i in 0..27u32 {
        let vf = make_gradient_frame(w, h, i);
        let pkts = enc.encode(&vf, i == 0).unwrap();
        if let Some((sps, pps)) = enc.sps_pps() {
            mp4.set_h264_params(sps.to_vec(), pps.to_vec());
            mkv.set_h264_params(sps.to_vec(), pps.to_vec());
        }
        for p in pkts {
            mp4.push_video_frame(p.pts_ns, p.is_keyframe, &p.data).unwrap();
            mkv.push_video_frame(p.pts_ns, p.is_keyframe, &p.data).unwrap();
        }
    }

    mp4.finish().unwrap();
    mkv.finish().unwrap();

    let mp4_bytes = fs::read(&mp4_path).unwrap();
    assert!(mp4_bytes.len() > 2000, "MP4 output surprisingly small: {}", mp4_bytes.len());
    assert!(mp4_bytes.windows(4).any(|w| w == b"ftyp"));
    assert!(mp4_bytes.windows(4).any(|w| w == b"moov"));
    assert!(mp4_bytes.windows(4).any(|w| w == b"moof"));
    assert!(mp4_bytes.windows(4).any(|w| w == b"mdat"));
    assert!(mp4_bytes.windows(4).any(|w| w == b"mfra"));

    let mkv_bytes = fs::read(&mkv_path).unwrap();
    assert!(mkv_bytes.len() > 2000);
    assert_eq!(&mkv_bytes[..4], &[0x1a, 0x45, 0xdf, 0xa3]);
    assert!(mkv_bytes.windows(4).any(|w| w == [0x1f, 0x43, 0xb6, 0x75]));

    // If ffprobe happens to be installed on this dev box, run independent container verification
    if let Ok(out) = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_name,codec_type", "-of", "compact"])
        .arg(&mp4_path)
        .output()
    {
        if out.status.success() {
            let txt = String::from_utf8_lossy(&out.stdout);
            println!("ffprobe MP4:\n{txt}");
            assert!(txt.contains("h264"));
            assert!(txt.contains("opus"));
        }
    }
    if let Ok(out) = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_name,codec_type", "-of", "compact"])
        .arg(&mkv_path)
        .output()
    {
        if out.status.success() {
            let txt = String::from_utf8_lossy(&out.stdout);
            println!("ffprobe MKV:\n{txt}");
            assert!(txt.contains("h264"));
            assert!(txt.contains("opus"));
        }
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_snapshot_png_jpeg_webp_and_rav1e() {
    let dir = std::env::temp_dir().join(format!("vexrec_snap_test_{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);

    let mut img = PixelImage::new(64, 64, PixelFormat::Bgrx8);
    let b = img.as_mut_bytes();
    for y in 0..64u32 {
        for x in 0..64u32 {
            let i = ((y * 64 + x) * 4) as usize;
            b[i] = (x * 4) as u8;
            b[i + 1] = (y * 4) as u8;
            b[i + 2] = 180;
            b[i + 3] = 255;
        }
    }

    let p_png = dir.join("shot.png");
    let p_jpg = dir.join("shot.jpg");
    let p_webp = dir.join("shot.webp");
    save_snapshot(&img, &p_png, ImageFormat::Png, CompressionSpeed::Fast).unwrap();
    save_snapshot(&img, &p_jpg, ImageFormat::Jpeg, CompressionSpeed::Balanced).unwrap();
    save_snapshot(&img, &p_webp, ImageFormat::Webp, CompressionSpeed::Balanced).unwrap();

    assert_eq!(&fs::read(&p_png).unwrap()[..4], &[0x89, b'P', b'N', b'G']);
    assert_eq!(&fs::read(&p_jpg).unwrap()[..3], &[0xff, 0xd8, 0xff]);
    let webp = fs::read(&p_webp).unwrap();
    assert_eq!(&webp[..4], b"RIFF");
    assert_eq!(&webp[8..12], b"WEBP");

    let mut av1 = Rav1eAv1Encoder::new(64, 64, 30, VideoQuality::Medium).unwrap();
    for i in 0..4 {
        let vf = make_gradient_frame(64, 64, i);
        let _ = av1.encode(&vf, i == 0).unwrap();
    }
    let tail = av1.flush().unwrap();
    assert!(!tail.is_empty());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_live_short_recording_if_display_available() {
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return;
    }

    let dir = std::env::temp_dir().join(format!("vexrec_live_rec_{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let out_mp4 = dir.join("live.mp4");

    let mut cfg = VexrecConfig::default();
    cfg.recording.framerate = 30;
    cfg.recording.container = ContainerFormat::Mp4;
    cfg.recording.encoder_preference = EncoderBackendPreference::Auto;

    let pipe = match RecordingPipeline::start(
        cfg,
        out_mp4.clone(),
        Some(Rect::new(0, 0, 320, 240)),
        false,
        false,
    ) {
        Ok(p) => p,
        Err(e) => {
            println!("Skipping live capture test cleanly: {e}");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
    };

    std::thread::sleep(std::time::Duration::from_millis(350));
    pipe.pause();
    std::thread::sleep(std::time::Duration::from_millis(100));
    pipe.resume();
    std::thread::sleep(std::time::Duration::from_millis(200));

    let summary = pipe.stop().unwrap();
    println!("Live recording summary: {:?}", summary);
    assert!(summary.frames >= 3, "Expected >= 3 frames, got {}", summary.frames);
    assert!(summary.bytes_written > 1000);

    if let Ok(out) = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_name,width,height", "-of", "compact"])
        .arg(&out_mp4)
        .output()
    {
        if out.status.success() {
            println!("ffprobe live MP4: {}", String::from_utf8_lossy(&out.stdout));
        }
    }
    if let Ok(dec) = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&out_mp4)
        .args(["-f", "null", "-"])
        .output()
    {
        let err_s = String::from_utf8_lossy(&dec.stderr);
        assert!(
            dec.status.success() && err_s.trim().is_empty(),
            "Live MP4 decode had warnings/errors: {err_s}"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_vaapi_raw_bitstream_and_ffmpeg_decode() {
    if probe_vaapi_h264().is_none() {
        return;
    }
    let mut enc = vexrec_media::encoder::vaapi::VaApiH264Encoder::new(
        320,
        240,
        30,
        VideoQuality::Medium,
    )
    .expect("VA-API init");
    let mut annexb = Vec::new();
    for i in 0..5u32 {
        let f = make_gradient_frame(320, 240, i);
        let pkts = enc.encode(&f, i == 0).expect("encode frame");
        for p in pkts {
            let head: String = p
                .data
                .iter()
                .take(48)
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            println!("VAAPI frame {i} len={} head: {head}", p.data.len());
            annexb.extend_from_slice(&p.data);
        }
    }
    let h264_path = std::env::temp_dir().join(format!("vexrec_va_raw_{}.h264", std::process::id()));
    fs::write(&h264_path, &annexb).unwrap();
    let dec = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "h264", "-i"])
        .arg(&h264_path)
        .args(["-f", "null", "-"])
        .output()
        .expect("ffmpeg run");
    let _ = fs::remove_file(&h264_path);
    let err_txt = String::from_utf8_lossy(&dec.stderr);
    println!("ffmpeg decode stderr: {err_txt}");
    assert!(
        dec.status.success() && err_txt.trim().is_empty(),
        "VA-API H.264 failed clean ffmpeg decode: {err_txt}"
    );
}

