use std::env;
use std::ffi::OsString;
use std::fmt::{self, Write as _};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tracing::field::{Field, Visit};
use tracing::{span, Level, Metadata, Subscriber};
use vexrec_core::config::VexrecConfig;
use vexrec_core::formats::{
    ContainerFormat, EncoderBackendPreference, ImageFormat, VideoQuality,
};
use vexrec_core::geometry::Rect;
use vexrec_core::xdg;
use vexrec_display::connect_auto;
use vexrec_media::audio::pulse_simple::is_pulse_available;
use vexrec_media::capture::is_pipewire_available;
use vexrec_media::encoder::nvenc::probe_nvenc_runtime;
use vexrec_media::encoder::vaapi::probe_vaapi_h264;
use vexrec_media::encoder::vulkan_video::probe_vulkan_video;
use vexrec_media::recorder::RecordingPipeline;
use vexrec_media::snapshot::save_snapshot;
use vexrec_os::dbus::portal_screenshot;
use vexrec_os::drm::enumerate_drm_devices;
use vexrec_os::ipc::{primary_socket_path, send_fast_opcode, IpcOpCode};
use vexrec_ui::build_output_path;

static STOP_SIGNAL: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sig(_sig: libc::c_int) {
    STOP_SIGNAL.store(true, Ordering::SeqCst);
}

struct StderrTracer {
    max_level: Level,
}

impl StderrTracer {
    fn install() {
        let max_level = match env::var("VEXREC_LOG")
            .or_else(|_| env::var("RUST_LOG"))
            .unwrap_or_else(|_| "info".into())
            .to_ascii_lowercase()
            .as_str()
        {
            "trace" => Level::TRACE,
            "debug" => Level::DEBUG,
            "warn" => Level::WARN,
            "error" => Level::ERROR,
            _ => Level::INFO,
        };
        let _ = tracing::subscriber::set_global_default(Self { max_level });
    }
}

struct LineVisitor(String);

impl Visit for LineVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            if self.0.is_empty() {
                let _ = write!(self.0, "{value:?}");
            } else {
                let _ = write!(self.0, " {value:?}");
            }
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

impl Subscriber for StderrTracer {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        *metadata.level() <= self.max_level
    }

    fn new_span(&self, _span: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut visitor = LineVisitor(String::new());
        event.record(&mut visitor);
        let meta = event.metadata();
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "[{} {}] {}", meta.level(), meta.target(), visitor.0);
    }

    fn enter(&self, _span: &span::Id) {}

    fn exit(&self, _span: &span::Id) {}
}

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().collect();
    if let Some(code) = try_fast_ipc_or_daemon(&args) {
        return code;
    }

    StderrTracer::install();
    run_full_cli(&args)
}

/// Ultra-fast `<0.3 ms` pre-parser hot path bypassing heavy initialization for IPC / daemon triggers.
fn try_fast_ipc_or_daemon(args: &[OsString]) -> Option<ExitCode> {
    let arg1 = args.get(1).and_then(|s| s.to_str());
    match (args.len(), arg1) {
        (1, None) | (2, Some("freeze")) => {
            if send_fast_opcode(IpcOpCode::Freeze, Duration::from_secs(2)).is_ok() {
                return Some(ExitCode::SUCCESS);
            }
            StderrTracer::install();
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "start", "vexrec-daemon.service"])
                .status();

            for _ in 0..45 {
                thread::sleep(Duration::from_millis(35));
                if send_fast_opcode(IpcOpCode::Freeze, Duration::from_secs(1)).is_ok() {
                    return Some(ExitCode::SUCCESS);
                }
            }

            eprintln!("Daemon socket unavailable; starting Vexrec overlay in-process...");
            match vexrec_ui::DaemonApp::initialize() {
                Ok(mut app) => {
                    app.open_overlay();
                    if let Err(e) = app.run() {
                        eprintln!("Daemon error: {e}");
                        return Some(ExitCode::FAILURE);
                    }
                    Some(ExitCode::SUCCESS)
                }
                Err(e) => {
                    eprintln!("Failed to start Vexrec: {e}");
                    Some(ExitCode::FAILURE)
                }
            }
        }
        (2, Some("stop-recording")) => {
            match send_fast_opcode(IpcOpCode::StopRecording, Duration::from_secs(6)) {
                Ok(resp) => {
                    println!("{resp}");
                    Some(ExitCode::SUCCESS)
                }
                Err(e) => {
                    eprintln!("Failed to stop recording: {e}");
                    Some(ExitCode::FAILURE)
                }
            }
        }
        (2, Some("cancel-freeze")) => {
            match send_fast_opcode(IpcOpCode::CancelFreeze, Duration::from_secs(2)) {
                Ok(resp) => {
                    println!("{resp}");
                    Some(ExitCode::SUCCESS)
                }
                Err(e) => {
                    eprintln!("Failed to cancel freeze overlay: {e}");
                    Some(ExitCode::FAILURE)
                }
            }
        }
        (2, Some("toggle-pause")) => {
            match send_fast_opcode(IpcOpCode::TogglePause, Duration::from_secs(2)) {
                Ok(resp) => {
                    println!("{resp}");
                    Some(ExitCode::SUCCESS)
                }
                Err(e) => {
                    eprintln!("Failed to toggle pause: {e}");
                    Some(ExitCode::FAILURE)
                }
            }
        }
        (2, Some("ping")) => {
            match send_fast_opcode(IpcOpCode::Ping, Duration::from_millis(500)) {
                Ok(resp) => {
                    println!("{resp}");
                    Some(ExitCode::SUCCESS)
                }
                Err(e) => {
                    eprintln!("Daemon not reachable: {e}");
                    Some(ExitCode::FAILURE)
                }
            }
        }
        (2, Some("settings")) => {
            let res: std::io::Result<String> = (|| {
                let mut stream = std::os::unix::net::UnixStream::connect(primary_socket_path())?;
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                stream.write_all(b"open_settings\n")?;
                let mut resp = String::new();
                let _ = std::io::Read::read_to_string(&mut stream, &mut resp);
                Ok(resp.trim().to_string())
            })();
            match res {
                Ok(resp) => {
                    println!("{resp}");
                    Some(ExitCode::SUCCESS)
                }
                Err(e) => {
                    eprintln!("Failed to open settings: {e}");
                    Some(ExitCode::FAILURE)
                }
            }
        }
        (2, Some("daemon")) => {
            StderrTracer::install();
            println!("Starting Vexrec bare-metal OS daemon...");
            if let Err(e) = vexrec_ui::run_daemon() {
                eprintln!("Daemon exited with error: {e}");
                return Some(ExitCode::FAILURE);
            }
            Some(ExitCode::SUCCESS)
        }
        _ => None,
    }
}

fn run_full_cli(args: &[OsString]) -> ExitCode {
    let sub = args.get(1).and_then(|s| s.to_str()).unwrap_or("");
    let rest: Vec<&str> = args.iter().skip(2).filter_map(|s| s.to_str()).collect();

    match sub {
        "-h" | "--help" | "help" => {
            print_help();
            ExitCode::SUCCESS
        }
        "-V" | "--version" | "version" => {
            println!("vexrec {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "doctor" => run_doctor(),
        "screenshot" => match run_screenshot_cmd(&rest) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("Screenshot failed: {e}");
                ExitCode::FAILURE
            }
        },
        "record" => match run_record_cmd(&rest) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("Recording failed: {e}");
                ExitCode::FAILURE
            }
        },
        "completions" => match run_completions_cmd(&rest) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("Completions failed: {e}");
                ExitCode::FAILURE
            }
        },
        other => {
            eprintln!("Unknown command '{other}'. Run 'vexrec --help' for usage.");
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    println!(
        "vexrec {} - Pure-Rust bare-metal Linux screenshot & screen recorder\n\n\
         USAGE:\n    vexrec [COMMAND]\n\n\
         COMMANDS:\n    \
           freeze            Trigger the interactive freeze capture overlay (default)\n    \
           stop-recording    Stop active daemon recording\n    \
           cancel-freeze     Hide active freeze overlay\n    \
           toggle-pause      Pause or resume active recording\n    \
           settings          Open preferences window in running daemon\n    \
           daemon            Start background epoll display/IPC daemon\n    \
           screenshot        Capture a direct screenshot from CLI\n    \
           record            Record desktop + audio directly from CLI\n    \
           doctor            Inspect display session, DRM, VA-API, Vulkan Video, NVENC, PipeWire\n    \
           completions       Generate or install bash/zsh/fish shell completions\n    \
           ping              Check running daemon health\n\n\
         Run 'vexrec screenshot --help' or 'vexrec record --help' for command options.",
        env!("CARGO_PKG_VERSION")
    );
}

fn run_doctor() -> ExitCode {
    println!("=== Vexrec Diagnostic Report (Pure-Rust OS Stack) ===");
    println!(
        "Session Type: {:?}",
        env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown".into())
    );
    println!("DISPLAY: {:?}", env::var("DISPLAY").ok());
    println!("WAYLAND_DISPLAY: {:?}", env::var("WAYLAND_DISPLAY").ok());

    match connect_auto() {
        Ok(b) => {
            let sz = b.primary_size();
            println!(
                "Display Backend: {} ({}x{}, {} monitor(s))",
                b.session_kind().name(),
                sz.width,
                sz.height,
                b.monitors().len()
            );
        }
        Err(e) => println!("Display Backend: unavailable ({e})"),
    }

    let drms = enumerate_drm_devices();
    println!("DRM Devices (sysfs probe): {}", drms.len());
    for d in &drms {
        println!(
            "  card={} render={:?} driver={} vendor=0x{:04x} boot_vga={} connected={} suspended={}",
            d.card_path.display(),
            d.render_path.as_ref().map(|p| p.display().to_string()),
            d.driver,
            d.vendor_id,
            d.is_boot_vga,
            d.has_connected_output,
            d.runtime_suspended
        );
    }

    println!("VA-API H.264: {:?}", probe_vaapi_h264());
    println!("Vulkan Video: {:?}", probe_vulkan_video());
    println!("NVENC Runtime: {}", probe_nvenc_runtime());
    println!("PipeWire Runtime: {}", is_pipewire_available());
    println!("PulseAudio Simple Runtime: {}", is_pulse_available());
    let cfg_path = VexrecConfig::config_path();
    println!(
        "Config Path: {} (exists: {})",
        cfg_path.display(),
        cfg_path.exists()
    );
    ExitCode::SUCCESS
}

fn run_screenshot_cmd(args: &[&str]) -> std::result::Result<(), String> {
    let mut interactive = false;
    let mut portal = false;
    let mut clipboard = false;
    let mut crop: Option<Rect> = None;
    let mut fmt_override: Option<ImageFormat> = None;
    let mut output: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "-h" | "--help" => {
                println!(
                    "USAGE:\n    vexrec screenshot [OPTIONS]\n\n\
                     OPTIONS:\n    \
                       -i, --interactive       Interactive Portal screenshot\n    \
                           --portal            Force XDG Desktop Portal screenshot\n    \
                           --x11               Use direct display backend capture\n    \
                       -c, --clipboard         Also place screenshot onto session clipboard\n    \
                           --crop X,Y,W,H      Crop rectangle\n    \
                       -f, --format PNG|JPG|WEBP  Output format\n    \
                       -o, --output PATH       Explicit output file path"
                );
                return Ok(());
            }
            "-i" | "--interactive" => interactive = true,
            "--portal" => portal = true,
            "--x11" => portal = false,
            "-c" | "--clipboard" => clipboard = true,
            "--crop" => {
                i += 1;
                let v = args.get(i).ok_or("--crop requires X,Y,W,H")?;
                crop = Some(parse_crop_rect(v).ok_or("Invalid --crop format, expected X,Y,W,H")?);
            }
            "-f" | "--format" => {
                i += 1;
                let v = args.get(i).ok_or("--format requires png, jpg, or webp")?.to_ascii_lowercase();
                fmt_override = Some(match v.as_str() {
                    "jpg" | "jpeg" => ImageFormat::Jpeg,
                    "webp" => ImageFormat::Webp,
                    "png" => ImageFormat::Png,
                    _ => return Err(format!("Unsupported screenshot format: {v}")),
                });
            }
            "-o" | "--output" => {
                i += 1;
                let v = args.get(i).ok_or("--output requires a path")?;
                output = Some(expand_path(Path::new(v)));
            }
            unknown => return Err(format!("Unknown screenshot option: {unknown}")),
        }
        i += 1;
    }

    let config = VexrecConfig::load().unwrap_or_default();
    let fmt = output
        .as_ref()
        .map(|p| ImageFormat::from_path(p))
        .or(fmt_override)
        .unwrap_or(config.screenshot.default_format);

    let out_path = output.unwrap_or_else(|| {
        let dir = expand_path(&config.storage.screenshot_dir);
        let _ = fs::create_dir_all(&dir);
        build_output_path(
            &dir,
            &config.storage.filename_pattern,
            "Screenshot",
            fmt.extension(),
        )
    });

    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    // Cancel any open freeze overlay so it never appears in CLI screenshots
    if send_fast_opcode(IpcOpCode::CancelFreeze, Duration::from_millis(200)).is_ok() {
        thread::sleep(Duration::from_millis(50));
    }

    if portal {
        let uri_or_path = portal_screenshot(interactive).map_err(|e| e.to_string())?;
        let src_path = uri_or_path.strip_prefix("file://").unwrap_or(&uri_or_path);
        fs::copy(src_path, &out_path).map_err(|e| format!("Failed to copy portal capture: {e}"))?;
        println!("Screenshot saved to: {}", out_path.display());
        return Ok(());
    }

    let mut backend = connect_auto().map_err(|e| e.to_string())?;
    let img = backend
        .capture_screen(crop, config.recording.show_cursor)
        .map_err(|e| e.to_string())?;

    if clipboard {
        let _ = backend.set_clipboard_image(img.clone());
        let _ = backend.poll_events();
    }

    save_snapshot(&img, &out_path, fmt, config.screenshot.compression_level)
        .map_err(|e| e.to_string())?;
    println!("Screenshot saved to: {}", out_path.display());
    Ok(())
}

fn run_record_cmd(args: &[&str]) -> std::result::Result<(), String> {
    let mut cfg = VexrecConfig::load().unwrap_or_default();
    let mut crop: Option<Rect> = None;
    let mut duration_secs: Option<u64> = None;
    let mut output: Option<PathBuf> = None;
    let mut mic = cfg.audio.capture_microphone;
    let mut desktop_audio = cfg.audio.capture_desktop;

    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "-h" | "--help" => {
                println!(
                    "USAGE:\n    vexrec record [OPTIONS]\n\n\
                     OPTIONS:\n    \
                       -f, --fps N              Target framerate\n    \
                           --format MP4|MKV|WEBM|GIF  Container format\n    \
                           --encoder auto|vaapi|vulkan|nvenc|cpu\n    \
                           --quality low|medium|high|lossless\n    \
                           --crop X,Y,W,H       Capture crop rectangle\n    \
                           --mic / --no-mic     Enable/disable microphone\n    \
                           --desktop-audio / --no-desktop-audio\n    \
                           --duration SECS      Auto-stop after N seconds\n    \
                       -o, --output PATH        Output video path"
                );
                return Ok(());
            }
            "-f" | "--fps" => {
                i += 1;
                cfg.recording.framerate = args
                    .get(i)
                    .ok_or("--fps requires a number")?
                    .parse::<u32>()
                    .map_err(|_| "Invalid --fps number")?;
            }
            "--format" | "--container" => {
                i += 1;
                let v = args.get(i).ok_or("--format requires container")?.to_ascii_lowercase();
                cfg.recording.container = match v.as_str() {
                    "mp4" => ContainerFormat::Mp4,
                    "mkv" => ContainerFormat::Mkv,
                    "webm" => ContainerFormat::Webm,
                    "gif" => ContainerFormat::Gif,
                    _ => return Err(format!("Unsupported container: {v}")),
                };
            }
            "--encoder" => {
                i += 1;
                let v = args.get(i).ok_or("--encoder requires value")?.to_ascii_lowercase();
                cfg.recording.encoder_preference = match v.as_str() {
                    "auto" => EncoderBackendPreference::Auto,
                    "vaapi" => EncoderBackendPreference::VaApi,
                    "vulkan" | "vulkan_video" => EncoderBackendPreference::VulkanVideo,
                    "nvenc" => EncoderBackendPreference::Nvenc,
                    "cpu" | "sw" => EncoderBackendPreference::Cpu,
                    _ => return Err(format!("Unsupported encoder preference: {v}")),
                };
            }
            "--quality" => {
                i += 1;
                let v = args.get(i).ok_or("--quality requires value")?.to_ascii_lowercase();
                cfg.recording.quality = match v.as_str() {
                    "low" => VideoQuality::Low,
                    "medium" => VideoQuality::Medium,
                    "high" => VideoQuality::High,
                    "lossless" => VideoQuality::Lossless,
                    _ => return Err(format!("Unsupported quality: {v}")),
                };
            }
            "--crop" => {
                i += 1;
                let v = args.get(i).ok_or("--crop requires X,Y,W,H")?;
                crop = Some(parse_crop_rect(v).ok_or("Invalid --crop, expected X,Y,W,H")?);
            }
            "--mic" => mic = true,
            "--no-mic" => mic = false,
            "--desktop-audio" => desktop_audio = true,
            "--no-desktop-audio" => desktop_audio = false,
            "--duration" => {
                i += 1;
                duration_secs = Some(
                    args.get(i)
                        .ok_or("--duration requires seconds")?
                        .parse::<u64>()
                        .map_err(|_| "Invalid --duration seconds")?,
                );
            }
            "-o" | "--output" => {
                i += 1;
                let v = args.get(i).ok_or("--output requires path")?;
                output = Some(expand_path(Path::new(v)));
            }
            unknown => return Err(format!("Unknown record option: {unknown}")),
        }
        i += 1;
    }

    let out_path = output.unwrap_or_else(|| {
        let dir = expand_path(&cfg.storage.recording_dir);
        let _ = fs::create_dir_all(&dir);
        build_output_path(
            &dir,
            &cfg.storage.filename_pattern,
            "Recording",
            cfg.recording.container.extension(),
        )
    });

    STOP_SIGNAL.store(false, Ordering::SeqCst);
    unsafe {
        libc::signal(libc::SIGINT, handle_sig as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handle_sig as *const () as libc::sighandler_t);
    }

    println!("Starting recording -> {}", out_path.display());
    let pipeline = RecordingPipeline::start(cfg, out_path, crop, mic, desktop_audio)
        .map_err(|e| e.to_string())?;
    let start = Instant::now();
    println!("Recording active. Press Ctrl+C to stop cleanly...");

    while !STOP_SIGNAL.load(Ordering::SeqCst) {
        if let Some(max_s) = duration_secs {
            if start.elapsed() >= Duration::from_secs(max_s) {
                break;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }

    println!("Finalizing recording container...");
    let summary = pipeline.stop().map_err(|e| e.to_string())?;
    println!(
        "Recording saved: {} ({}s, {} frames, {} bytes, {})",
        summary.path.display(),
        summary.duration_secs,
        summary.frames,
        summary.bytes_written,
        summary.encoder_backend
    );
    Ok(())
}

fn run_completions_cmd(args: &[&str]) -> std::result::Result<(), String> {
    let install = args.iter().any(|&a| a == "-i" || a == "--install");
    let shell = args
        .iter()
        .copied()
        .find(|a| !a.starts_with('-'));

    if install {
        let home = xdg::home_dir();
        let bash_dir = home.join(".local/share/bash-completion/completions");
        fs::create_dir_all(&bash_dir).map_err(|e| e.to_string())?;
        fs::write(bash_dir.join("vexrec"), bash_completions()).map_err(|e| e.to_string())?;

        let fish_dir = home.join(".config/fish/completions");
        fs::create_dir_all(&fish_dir).map_err(|e| e.to_string())?;
        fs::write(fish_dir.join("vexrec.fish"), fish_completions()).map_err(|e| e.to_string())?;

        let zsh_dir = home.join(".local/share/zsh/site-functions");
        fs::create_dir_all(&zsh_dir).map_err(|e| e.to_string())?;
        fs::write(zsh_dir.join("_vexrec"), zsh_completions()).map_err(|e| e.to_string())?;

        println!("Installed bash, fish, and zsh completions for vexrec.");
        return Ok(());
    }

    match shell {
        Some("bash") => print!("{}", bash_completions()),
        Some("fish") => print!("{}", fish_completions()),
        Some("zsh") => print!("{}", zsh_completions()),
        _ => return Err("Specify bash, zsh, or fish, or pass --install".into()),
    }
    Ok(())
}

fn bash_completions() -> &'static str {
    "_vexrec() {\n    local cur=\"${COMP_WORDS[COMP_CWORD]}\"\n    if [[ ${COMP_CWORD} -eq 1 ]]; then\n        COMPREPLY=( $(compgen -W \"freeze stop-recording cancel-freeze toggle-pause settings daemon screenshot record doctor completions ping help\" -- \"${cur}\") )\n    fi\n}\ncomplete -F _vexrec vexrec\n"
}

fn zsh_completions() -> &'static str {
    "#compdef vexrec\n_vexrec() {\n    local -a cmds\n    cmds=('freeze:Open capture overlay' 'stop-recording:Stop active recording' 'cancel-freeze:Hide capture overlay' 'toggle-pause:Pause or resume recording' 'settings:Open preferences window' 'daemon:Run background daemon' 'screenshot:Direct screenshot capture' 'record:Direct screen recording' 'doctor:Inspect system codecs and display' 'completions:Generate shell completions' 'ping:Ping daemon')\n    _describe 'command' cmds\n}\n_vexrec \"$@\"\n"
}

fn fish_completions() -> &'static str {
    "complete -c vexrec -f\ncomplete -c vexrec -n '__fish_use_subcommand' -a 'freeze stop-recording cancel-freeze toggle-pause settings daemon screenshot record doctor completions ping help'\n"
}

fn parse_crop_rect(s: &str) -> Option<Rect> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        return None;
    }
    let x = parts[0].trim().parse::<i32>().ok()?;
    let y = parts[1].trim().parse::<i32>().ok()?;
    let width = parts[2].trim().parse::<u32>().ok()?;
    let height = parts[3].trim().parse::<u32>().ok()?;
    Some(Rect::new(x, y, width, height))
}

fn expand_path(p: &Path) -> PathBuf {
    vexrec_core::expand_tilde(p)
}
