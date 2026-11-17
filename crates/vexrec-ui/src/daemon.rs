use std::io::{Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use polling::{Event, Events, Poller};
use tracing::{error, info, warn};

use vexrec_core::buffer::PixelImage;
use vexrec_core::config::{ensure_unique_path, VexrecConfig};
use vexrec_core::error::{Result, VexrecError};
use vexrec_core::formats::{CompressionSpeed, ImageFormat};
use vexrec_core::geometry::Rect;
use vexrec_display::{
    connect_auto, DisplayBackend, DisplayEvent, HotkeyAction, WindowRole,
};
use vexrec_media::recorder::{RecordingPipeline, RecordingSummary};
use vexrec_media::snapshot::save_snapshot;
use vexrec_os::dbus::{
    portal_choose_folder, portal_save_file, spawn_portal_global_shortcuts, NotificationHub,
};
use vexrec_os::fonts::FontEngine;
use vexrec_os::ipc::{bind_daemon_listener, read_incoming_command, IpcOpCode};
use vexrec_os::tray::{TrayAction, TrayIndicator};

use crate::frame::FrameController;
use crate::indicator::IndicatorController;
use crate::overlay::{FreezeOverlayController, OverlayOutcome};
use crate::settings::{SettingsController, SettingsOutcome};
use crate::state::TargetMode;

const KEY_DISPLAY: usize = 1;
const KEY_IPC: usize = 2;
const KEY_WAKE: usize = 3;

enum DaemonBgMsg {
    GlobalShortcut(String),
    ScreenshotDirPicked(PathBuf),
    RecordingDirPicked(PathBuf),
    SaveAsPathPicked(Option<PathBuf>),
    TrayAction(TrayAction),
}

/// Zero-GTK, zero-tokio bare-metal daemon reactor multiplexed over Linux `epoll` (`polling`).
pub struct DaemonApp {
    backend: Box<dyn DisplayBackend>,
    config: VexrecConfig,
    overlay: FreezeOverlayController,
    indicator: IndicatorController,
    frame: FrameController,
    settings: SettingsController,
    recorder: Option<RecordingPipeline>,
    notif: Arc<NotificationHub>,
    tray: TrayIndicator,
    ipc_listener: UnixListener,
    wake_rx: UnixStream,
    wake_tx: UnixStream,
    msg_rx: Receiver<DaemonBgMsg>,
    msg_tx: Sender<DaemonBgMsg>,
    pending_save_as_img: Option<PixelImage>,
    running: bool,
}

impl DaemonApp {
    pub fn initialize() -> Result<Self> {
        let config = VexrecConfig::load().unwrap_or_default();
        let fonts = Arc::new(FontEngine::load_system_fonts());
        let mut backend = connect_auto()?;
        backend.register_global_hotkeys();

        let ipc_listener = bind_daemon_listener()?;
        ipc_listener
            .set_nonblocking(true)
            .map_err(|e| VexrecError::Ipc(format!("Failed to set nonblocking IPC socket: {e}")))?;

        let (wake_tx, wake_rx) = UnixStream::pair()
            .map_err(|e| VexrecError::Ipc(format!("Failed to create daemon wakeup pipe: {e}")))?;
        let _ = wake_tx.set_nonblocking(true);
        let _ = wake_rx.set_nonblocking(true);

        let (msg_tx, msg_rx) = mpsc::channel();
        let notif = Arc::new(NotificationHub::start());
        let tx_tray = msg_tx.clone();
        let wake_tray = wake_tx.try_clone().ok();
        let tray = TrayIndicator::start(move |action| {
            let _ = tx_tray.send(DaemonBgMsg::TrayAction(action));
            if let Some(ref w) = wake_tray {
                let _ = (&*w).write_all(&[1u8]);
            }
        });

        if backend.session_kind().is_wayland() {
            let tx_portal = msg_tx.clone();
            if let Ok(w_portal) = wake_tx.try_clone() {
                spawn_portal_global_shortcuts(move |id| {
                    let _ = tx_portal.send(DaemonBgMsg::GlobalShortcut(id));
                    let _ = (&w_portal).write_all(&[1u8]);
                });
            }
        }

        info!(
            session = ?backend.session_kind(),
            "Vexrec native OS daemon initialized (zero GTK / zero tokio)"
        );

        Ok(Self {
            backend,
            config,
            overlay: FreezeOverlayController::new(Arc::clone(&fonts)),
            indicator: IndicatorController::new(Arc::clone(&fonts)),
            frame: FrameController::new(Arc::clone(&fonts)),
            settings: SettingsController::new(fonts),
            recorder: None,
            notif,
            tray,
            ipc_listener,
            wake_rx,
            wake_tx,
            msg_rx,
            msg_tx,
            pending_save_as_img: None,
            running: true,
        })
    }

    pub fn run(&mut self) -> Result<()> {
        let poller = Poller::new()
            .map_err(|e| VexrecError::Display(format!("Failed to initialize epoll poller: {e}")))?;

        let display_fd = self.backend.raw_fd();
        let ipc_fd = self.ipc_listener.as_raw_fd();
        let wake_fd = self.wake_rx.as_raw_fd();

        unsafe {
            poller.add(display_fd, Event::readable(KEY_DISPLAY))?;
            poller.add(ipc_fd, Event::readable(KEY_IPC))?;
            poller.add(wake_fd, Event::readable(KEY_WAKE))?;
        }

        let mut events = Events::new();
        let mut last_live_refresh = std::time::Instant::now();

        while self.running {
            self.drain_display_events();
            self.accept_ipc_connections();
            self.drain_wake_pipe();

            if self.overlay.is_animating() {
                self.overlay.tick(&mut *self.backend);
            }
            if self.overlay.needs_live_window_refresh()
                && last_live_refresh.elapsed() >= Duration::from_millis(40)
            {
                self.overlay.refresh_live_windows(&mut *self.backend);
                last_live_refresh = std::time::Instant::now();
            }
            if let Some(rec) = &self.recorder {
                let secs = rec.elapsed_seconds();
                let paused = rec.is_paused();
                self.sync_recording_indicators(secs, paused);
            }

            if !self.running {
                break;
            }

            unsafe {
                let _ = poller.modify(
                    BorrowedFd::borrow_raw(display_fd),
                    Event::readable(KEY_DISPLAY),
                );
                let _ = poller.modify(BorrowedFd::borrow_raw(ipc_fd), Event::readable(KEY_IPC));
                let _ = poller.modify(BorrowedFd::borrow_raw(wake_fd), Event::readable(KEY_WAKE));
            }

            let timeout = if self.overlay.is_animating() {
                Some(Duration::from_millis(8))
            } else if self.overlay.needs_live_window_refresh() {
                Some(Duration::from_millis(40))
            } else if self.recorder.is_some() {
                Some(Duration::from_millis(250))
            } else {
                None
            };

            events.clear();
            if let Err(e) = poller.wait(&mut events, timeout) {
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(VexrecError::Display(format!("Daemon poller wait failed: {e}")));
            }

            for ev in events.iter() {
                match ev.key {
                    KEY_DISPLAY => self.drain_display_events(),
                    KEY_IPC => self.accept_ipc_connections(),
                    KEY_WAKE => self.drain_wake_pipe(),
                    _ => {}
                }
            }
        }

        let _ = self.stop_recording();
        let _ = poller.delete(unsafe { BorrowedFd::borrow_raw(display_fd) });
        let _ = poller.delete(unsafe { BorrowedFd::borrow_raw(ipc_fd) });
        let _ = poller.delete(unsafe { BorrowedFd::borrow_raw(wake_fd) });
        Ok(())
    }

    fn drain_display_events(&mut self) {
        loop {
            let batch = self.backend.poll_events();
            if batch.is_empty() {
                break;
            }
            for ev in batch {
                self.handle_display_event(ev);
            }
        }
    }

    fn handle_display_event(&mut self, ev: DisplayEvent) {
        match ev {
            DisplayEvent::Expose { role, .. } => match role {
                WindowRole::OverlayFullscreen => self.overlay.on_expose(&mut *self.backend),
                WindowRole::IndicatorPill => self.indicator.on_expose(&mut *self.backend),
                WindowRole::RecordingFrame => self.frame.on_expose(&mut *self.backend),
                WindowRole::SettingsDialog => self.settings.on_expose(&mut *self.backend),
            },
            DisplayEvent::PointerMotion { role, x, y } => match role {
                WindowRole::OverlayFullscreen => {
                    self.overlay.on_pointer_motion(&mut *self.backend, x, y);
                }
                WindowRole::SettingsDialog => {
                    self.settings.on_pointer_motion(&mut *self.backend, x, y);
                }
                _ => {}
            },
            DisplayEvent::PointerLeave { role } => {
                if role == WindowRole::SettingsDialog {
                    self.settings.on_pointer_leave(&mut *self.backend);
                }
            }
            DisplayEvent::PointerButton {
                role,
                button,
                pressed,
                x,
                y,
            } => match role {
                WindowRole::OverlayFullscreen => {
                    let out =
                        self.overlay
                            .on_pointer_button(&mut *self.backend, button, pressed, x, y);
                    self.handle_overlay_outcome(out);
                }
                WindowRole::IndicatorPill => {
                    if button == 1 && !pressed {
                        let _ = self.stop_recording();
                    }
                }
                WindowRole::SettingsDialog => {
                    let out =
                        self.settings
                            .on_pointer_button(&mut *self.backend, button, pressed, x, y);
                    self.handle_settings_outcome(out);
                }
                WindowRole::RecordingFrame => {}
            },
            DisplayEvent::KeyPress { role, key } => match role {
                WindowRole::OverlayFullscreen => {
                    let out = self.overlay.on_key(&mut *self.backend, key);
                    self.handle_overlay_outcome(out);
                }
                WindowRole::SettingsDialog => {
                    let out = self.settings.on_key(&mut *self.backend, key);
                    self.handle_settings_outcome(out);
                }
                _ => {}
            },
            DisplayEvent::WindowDamaged { window_id } => {
                self.overlay
                    .on_window_damaged(&mut *self.backend, window_id);
            }
            DisplayEvent::GlobalHotkey(HotkeyAction::Freeze) => {
                self.open_overlay();
            }
            DisplayEvent::GlobalHotkey(HotkeyAction::StopRecording) => {
                let _ = self.stop_recording();
            }
            DisplayEvent::CloseRequested { role } => match role {
                WindowRole::OverlayFullscreen => self.overlay.hide(&mut *self.backend),
                WindowRole::SettingsDialog => self.settings.hide(&mut *self.backend),
                WindowRole::IndicatorPill => {
                    let _ = self.stop_recording();
                }
                WindowRole::RecordingFrame => {}
            },
            DisplayEvent::SurfaceConfigured { role, .. } => match role {
                WindowRole::OverlayFullscreen => self.overlay.on_expose(&mut *self.backend),
                WindowRole::IndicatorPill => self.indicator.on_expose(&mut *self.backend),
                WindowRole::RecordingFrame => self.frame.on_expose(&mut *self.backend),
                WindowRole::SettingsDialog => self.settings.on_expose(&mut *self.backend),
            },
        }
    }

    fn accept_ipc_connections(&mut self) {
        loop {
            match self.ipc_listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                    match read_incoming_command(&mut stream) {
                        Ok((IpcOpCode::Freeze, _)) => {
                            let _ = stream.write_all(b"OK: freeze\n");
                            self.open_overlay();
                        }
                        Ok((IpcOpCode::CancelFreeze, _)) => {
                            self.overlay.hide(&mut *self.backend);
                            let _ = stream.write_all(b"OK: freeze cancelled\n");
                        }
                        Ok((IpcOpCode::StopRecording, _)) => match self.stop_recording() {
                            Ok(Some(sum)) => {
                                let msg = format!("OK: {}\n", sum.path.display());
                                let _ = stream.write_all(msg.as_bytes());
                            }
                            Ok(None) => {
                                let _ = stream.write_all(b"OK: no active recording\n");
                            }
                            Err(e) => {
                                let msg = format!("ERR: {e}\n");
                                let _ = stream.write_all(msg.as_bytes());
                            }
                        },
                        Ok((IpcOpCode::TogglePause, _)) => match self.toggle_recording_pause() {
                            Some(true) => {
                                let _ = stream.write_all(b"OK: paused\n");
                            }
                            Some(false) => {
                                let _ = stream.write_all(b"OK: resumed\n");
                            }
                            None => {
                                let _ = stream.write_all(b"OK: no active recording\n");
                            }
                        },
                        Ok((IpcOpCode::Ping, _)) => {
                            let _ = stream.write_all(b"pong\n");
                        }
                        Ok((IpcOpCode::CustomText, payload)) => {
                            let cmd = String::from_utf8_lossy(&payload);
                            let trimmed = cmd.trim();
                            match trimmed {
                                "open_settings" | "settings" => {
                                    let _ = stream.write_all(b"OK: settings\n");
                                    self.overlay.hide(&mut *self.backend);
                                    self.settings.show(&mut *self.backend, &self.config);
                                }
                                "quit" | "shutdown" => {
                                    let _ = stream.write_all(b"OK: shutting down\n");
                                    self.running = false;
                                }
                                other => {
                                    let reply = format!("UNKNOWN: {other}\n");
                                    let _ = stream.write_all(reply.as_bytes());
                                }
                            }
                        }
                        Err(e) => {
                            warn!("IPC client read error: {e}");
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    warn!("IPC accept error: {e}");
                    break;
                }
            }
        }
    }

    fn drain_wake_pipe(&mut self) {
        let mut buf = [0u8; 64];
        loop {
            match self.wake_rx.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }

        while let Ok(msg) = self.msg_rx.try_recv() {
            match msg {
                DaemonBgMsg::GlobalShortcut(id) => match id.as_str() {
                    "freeze" => self.open_overlay(),
                    "stop_recording" => {
                        let _ = self.stop_recording();
                    }
                    _ => {}
                },
                DaemonBgMsg::ScreenshotDirPicked(dir) => {
                    self.settings.set_screenshot_dir(&mut *self.backend, dir);
                }
                DaemonBgMsg::RecordingDirPicked(dir) => {
                    self.settings.set_recording_dir(&mut *self.backend, dir);
                }
                DaemonBgMsg::SaveAsPathPicked(maybe_path) => {
                    if let (Some(img), Some(path)) =
                        (self.pending_save_as_img.take(), maybe_path)
                    {
                        let fmt = ImageFormat::from_path(&path);
                        let speed = self.config.screenshot.compression_level;
                        self.spawn_save_snapshot(img, path, fmt, speed);
                    } else {
                        self.pending_save_as_img = None;
                    }
                }
                DaemonBgMsg::TrayAction(action) => match action {
                    TrayAction::StopRecording => {
                        let _ = self.stop_recording();
                    }
                    TrayAction::TogglePause => {
                        let _ = self.toggle_recording_pause();
                    }
                },
            }
        }
    }

    pub fn open_overlay(&mut self) {
        if self.overlay.is_visible() {
            return;
        }
        if self.settings.is_visible() {
            self.settings.hide(&mut *self.backend);
            self.backend.sync_and_wait_frame();
        }

        let desktop = match self
            .backend
            .capture_screen(None, self.config.recording.show_cursor)
        {
            Ok(img) => img,
            Err(e) => {
                error!("Failed to capture desktop for freeze overlay: {e}");
                return;
            }
        };

        let windows = self.backend.enumerate_windows(true);
        self.overlay
            .show(&mut *self.backend, &self.config, desktop, windows);
    }

    fn handle_overlay_outcome(&mut self, outcome: OverlayOutcome) {
        match outcome {
            OverlayOutcome::None => {}
            OverlayOutcome::Cancel => {
                self.persist_overlay_selection();
                self.overlay.hide(&mut *self.backend);
            }
            OverlayOutcome::OpenSettings => {
                self.persist_overlay_selection();
                self.overlay.hide(&mut *self.backend);
                self.settings.show(&mut *self.backend, &self.config);
            }
            OverlayOutcome::RefreshCapture => {
                self.persist_overlay_selection();
                self.overlay.hide(&mut *self.backend);
                self.backend.sync_and_wait_frame();
                self.open_overlay();
            }
            OverlayOutcome::CaptureScreenshot { img, save_as } => {
                self.persist_overlay_selection();
                self.overlay.hide(&mut *self.backend);
                self.backend.sync_and_wait_frame();

                if self.config.screenshot.copy_to_clipboard {
                    if let Err(e) = self.backend.set_clipboard_image(img.clone()) {
                        warn!("Clipboard image copy failed: {e}");
                    }
                }

                if save_as {
                    let ext = self.config.screenshot.default_format.extension();
                    let default_name = format!(
                        "{}.{ext}",
                        render_filename(
                            &self.config.storage.filename_pattern,
                            "Screenshot",
                        )
                    );
                    self.pending_save_as_img = Some(img);
                    let tx = self.msg_tx.clone();
                    if let Ok(mut wake) = self.wake_tx.try_clone() {
                        let _ = std::thread::Builder::new()
                            .name("vexrec-save-as".into())
                            .spawn(move || {
                                let picked = portal_save_file("Save Screenshot", &default_name)
                                    .ok()
                                    .flatten();
                                let _ = tx.send(DaemonBgMsg::SaveAsPathPicked(picked));
                                let _ = wake.write_all(&[1u8]);
                            });
                    }
                } else {
                    let fmt = self.config.screenshot.default_format;
                    let path = build_output_path(
                        &self.config.storage.screenshot_dir,
                        &self.config.storage.filename_pattern,
                        "Screenshot",
                        fmt.extension(),
                    );
                    let speed = self.config.screenshot.compression_level;
                    self.spawn_save_snapshot(img, path, fmt, speed);
                }
            }
            OverlayOutcome::StartRecording {
                crop,
                record_mic,
                record_desktop,
            } => {
                self.persist_overlay_selection();
                self.overlay.hide(&mut *self.backend);
                self.backend.sync_and_wait_frame();

                if let Err(e) = self.start_recording(crop, record_mic, record_desktop) {
                    error!("Failed to start recording: {e}");
                }
            }
        }
    }

    fn handle_settings_outcome(&mut self, outcome: SettingsOutcome) {
        match outcome {
            SettingsOutcome::None => {}
            SettingsOutcome::Minimize => {
                self.backend.minimize_surface(WindowRole::SettingsDialog);
            }
            SettingsOutcome::Close => {
                self.settings.hide(&mut *self.backend);
            }
            SettingsOutcome::Saved(new_cfg) => {
                self.config = new_cfg;
                if let Err(e) = self.config.save(&VexrecConfig::config_path()) {
                    warn!("Failed to persist vexrec config: {e}");
                }
                self.settings.hide(&mut *self.backend);
            }
            SettingsOutcome::BrowseScreenshotDir => {
                let tx = self.msg_tx.clone();
                if let Ok(mut wake) = self.wake_tx.try_clone() {
                    let _ = std::thread::Builder::new()
                        .name("vexrec-dir-snap".into())
                        .spawn(move || {
                            if let Ok(Some(dir)) =
                                portal_choose_folder("Select Screenshots Folder")
                            {
                                let _ = tx.send(DaemonBgMsg::ScreenshotDirPicked(dir));
                                let _ = wake.write_all(&[1u8]);
                            }
                        });
                }
            }
            SettingsOutcome::BrowseRecordingDir => {
                let tx = self.msg_tx.clone();
                if let Ok(mut wake) = self.wake_tx.try_clone() {
                    let _ = std::thread::Builder::new()
                        .name("vexrec-dir-rec".into())
                        .spawn(move || {
                            if let Ok(Some(dir)) =
                                portal_choose_folder("Select Recordings Folder")
                            {
                                let _ = tx.send(DaemonBgMsg::RecordingDirPicked(dir));
                                let _ = wake.write_all(&[1u8]);
                            }
                        });
                }
            }
        }
    }

    fn persist_overlay_selection(&mut self) {
        let mode_str = match self.overlay.target_mode() {
            TargetMode::Fullscreen => "fullscreen",
            TargetMode::Selection => "selection",
            TargetMode::Window => "window",
            TargetMode::WindowRegion => "window_region",
        };
        let rect = self.overlay.last_selection_to_persist();
        if self.config.screenshot.last_mode.as_deref() != Some(mode_str)
            || (rect.is_some() && self.config.screenshot.last_region != rect)
        {
            self.config.screenshot.last_mode = Some(mode_str.to_string());
            if let Some(r) = rect {
                self.config.screenshot.last_region = Some(r);
            }
            let _ = self.config.save(&VexrecConfig::config_path());
        }
    }

    fn spawn_save_snapshot(
        &self,
        img: PixelImage,
        path: PathBuf,
        fmt: ImageFormat,
        speed: CompressionSpeed,
    ) {
        let notif = self.notif.clone();
        let _ = std::thread::Builder::new()
            .name("vexrec-snap-save".into())
            .spawn(move || {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match save_snapshot(&img, &path, fmt, speed) {
                    Ok(()) => {
                        let body = format!("Saved to {}", path.display());
                        notif.notify_saved("camera-photo", "Screenshot Saved", &body, path);
                    }
                    Err(e) => {
                        error!("Failed to save screenshot to {}: {e}", path.display());
                    }
                }
            });
    }

    fn start_recording(
        &mut self,
        crop: Option<Rect>,
        record_mic: bool,
        record_desktop: bool,
    ) -> Result<()> {
        if self.recorder.is_some() {
            let _ = self.stop_recording();
        }

        let ext = self.config.recording.container.extension();
        let out_path = build_output_path(
            &self.config.storage.recording_dir,
            &self.config.storage.filename_pattern,
            "Recording",
            ext,
        );

        let pipeline = RecordingPipeline::start(
            self.config.clone(),
            out_path,
            crop,
            record_mic,
            record_desktop,
        )?;
        self.recorder = Some(pipeline);

        self.tray.show_recording(0, false);
        if !self.tray.is_host_active() {
            self.indicator.show(&mut *self.backend);
        }
        self.frame.show(
            &mut *self.backend,
            crop,
            self.config.recording.show_recording_frame,
        );
        Ok(())
    }

    fn stop_recording(&mut self) -> Result<Option<RecordingSummary>> {
        self.tray.hide();
        self.indicator.hide(&mut *self.backend);
        self.frame.hide(&mut *self.backend);

        let Some(rec) = self.recorder.take() else {
            return Ok(None);
        };

        let summary = rec.stop()?;
        let body = format!(
            "{}s ({} frames, {})",
            summary.duration_secs, summary.frames, summary.encoder_backend
        );
        self.notif.notify_saved(
            "video-x-generic",
            "Recording Saved",
            &body,
            summary.path.clone(),
        );
        Ok(Some(summary))
    }

    fn sync_recording_indicators(&mut self, secs: u64, paused: bool) {
        self.tray.update_timer(secs, paused);
        if self.tray.is_host_active() {
            if self.indicator.is_visible() {
                self.indicator.hide(&mut *self.backend);
            }
        } else {
            if !self.indicator.is_visible() {
                self.indicator.show(&mut *self.backend);
            }
            self.indicator.update(&mut *self.backend, secs, paused);
        }
    }

    fn toggle_recording_pause(&mut self) -> Option<bool> {
        let (secs, paused) = {
            let rec = self.recorder.as_ref()?;
            if rec.is_paused() {
                rec.resume();
            } else {
                rec.pause();
            }
            (rec.elapsed_seconds(), rec.is_paused())
        };
        self.sync_recording_indicators(secs, paused);
        Some(paused)
    }
}

pub fn render_filename(pattern: &str, capture_type: &str) -> String {
    let dt = Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    pattern
        .replace("{type}", capture_type)
        .replace("{datetime}", &dt)
}

pub fn build_output_path(dir: &Path, pattern: &str, capture_type: &str, ext: &str) -> PathBuf {
    let base_dir = vexrec_core::expand_tilde(dir);
    let _ = std::fs::create_dir_all(&base_dir);
    let base = render_filename(pattern, capture_type);
    let candidate = base_dir.join(format!("{base}.{ext}"));
    ensure_unique_path(candidate)
}
