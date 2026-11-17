use vexrec_core::buffer::{AlignedVec, PixelFormat, PixelImage};
use vexrec_core::config::VexrecConfig;
use vexrec_core::geometry::{Point, Rect};

#[test]
fn test_default_config() {
    let config = VexrecConfig::default();
    assert_eq!(config.recording.framerate, 60);
    assert!(config.audio.capture_desktop);
    assert!(config.audio.capture_microphone);
    assert!(config.recording.hardware_accel);
    assert!(!config.recording.show_recording_frame);
}

#[test]
fn test_backwards_compat_config_toml_deserialization() {
    let legacy_toml = r#"
[general]
autostart = true
close_to_tray = true

[recording]
framerate = 60
container = "Mp4"
video_codec = "Auto"
quality = "Medium"
show_cursor = true
show_recording_frame = false
hardware_accel = true

[audio]
capture_desktop = true
capture_microphone = true
codec = "Auto"
desktop_volume = 1.0
microphone_volume = 1.0
noise_suppression = true

[camera]
enabled = false
shape = "Circle"
anchor = "BottomRight"
width = 280
height = 280
margin = 32
mirror = true

[screenshot]
default_format = "Png"
copy_to_clipboard = true
show_preview_hud = true
preview_hud_timeout_secs = 5
include_cursor = false
last_mode = "region"

[screenshot.last_region]
x = 377
y = 852
width = 411
height = 159

[storage]
screenshot_dir = "~/Pictures/Vexrec"
recording_dir = "~/Videos/Vexrec"
filename_pattern = "Vexrec_{type}_{datetime}"

[hotkeys]
screenshot_area = "<Super><Shift>4"
screenshot_fullscreen = "<Super><Shift>3"
record_area = "<Super><Shift>5"
record_fullscreen = "<Super><Shift>6"
toggle_pause = "<Super><Shift>Space"
stop_recording = "<Super><Shift>Escape"
"#;

    let parsed: VexrecConfig = toml::from_str(legacy_toml).expect("Should parse existing config.toml");
    assert!(!parsed.recording.show_recording_frame);
    assert!(parsed.recording.prefer_fragmented_mp4);
    assert!(parsed.recording.damage_driven_capture);
    assert!(parsed.screenshot.clipboard_lazy_encode);
    assert_eq!(parsed.screenshot.last_region, Some(Rect::new(377, 852, 411, 159)));
}

#[test]
fn test_geometry_containment_and_snapping() {
    let rect = Rect::new(10, 10, 100, 100);
    assert!(rect.contains(Point::new(50, 50)));
    assert!(!rect.contains(Point::new(5, 5)));

    let odd = Rect::new(-5, 11, 137, 55).snap_even();
    assert_eq!(odd, Rect::new(0, 11, 136, 54));

    let tiny = Rect::new(3, 4, 5, 9).snap_even();
    assert_eq!(tiny, Rect::new(3, 4, 32, 32));
}

#[test]
fn test_geometry_from_points_and_union() {
    let r1 = Rect::from_points(Point::new(100, 200), Point::new(20, 50));
    assert_eq!(r1, Rect::new(20, 50, 80, 150));

    let r2 = Rect::new(10, 10, 30, 30);
    assert_eq!(r1.union(r2), Rect::new(10, 10, 90, 190));
}

#[test]
fn test_aligned_vec_and_crop() {
    let buf = AlignedVec::zeroed(1024);
    assert_eq!(buf.as_slice().as_ptr() as usize % 64, 0);

    let mut img = PixelImage::new(20, 10, PixelFormat::Rgba8);
    let data = img.as_mut_bytes();
    for y in 0..10u32 {
        for x in 0..20u32 {
            let off = ((y * 20 + x) * 4) as usize;
            data[off] = x as u8;
            data[off + 1] = y as u8;
            data[off + 2] = 99;
            data[off + 3] = 255;
        }
    }
    let cropped = img.crop_rect(5, 3, 4, 2).unwrap();
    assert_eq!((cropped.width, cropped.height), (4, 2));
    assert_eq!(&cropped.as_bytes()[0..4], &[5, 3, 99, 255]);
}
