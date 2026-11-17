use vexrec_display::{connect_auto, detect_wayland_session_kind, SessionKind};

#[test]
fn test_display_auto_probe_and_capture_on_live_session() {
    let _kind = detect_wayland_session_kind();
    if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let mut backend = connect_auto().expect("Display backend should connect cleanly");
        let sz = backend.primary_size();
        assert!(sz.width > 0 && sz.height > 0);
        let monitors = backend.monitors();
        assert!(!monitors.is_empty());

        if backend.session_kind() == SessionKind::X11 {
            let t0 = std::time::Instant::now();
            let img = backend.capture_screen(None, false).expect("SHM screen capture failed");
            let elapsed = t0.elapsed();
            assert_eq!(img.width, sz.width);
            assert_eq!(img.height, sz.height);
            assert!(
                elapsed.as_millis() < 250,
                "Expected sub-250ms X11 SHM screen grab, got {:?}",
                elapsed
            );

            let wins = backend.enumerate_windows(false);
            let _ = wins.len();
        }
    }
}
