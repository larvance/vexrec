use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use vexrec_os::drm;
use vexrec_os::fonts::{FontEngine, FontRole};
use vexrec_os::ipc::{encode_frame_header, read_incoming_command, IpcOpCode};
use vexrec_os::shm::ShmBuffer;
use vexrec_os::simd::{bgra_to_nv12_bt709, bgra_to_nv12_bt709_scalar, bgrx_to_rgba, bgrx_to_rgba_scalar};

#[test]
fn test_simd_bgrx_to_rgba_equivalence_odd_widths() {
    for num_px in [1usize, 3, 7, 8, 9, 15, 16, 17, 33, 1365] {
        let mut src = Vec::with_capacity(num_px * 4);
        for i in 0..num_px {
            src.push((i * 3 % 256) as u8);
            src.push((i * 7 % 256) as u8);
            src.push((i * 11 % 256) as u8);
            src.push(0); // junk alpha in X11 BGRx
        }
        let mut dst_simd = vec![0u8; num_px * 4];
        let mut dst_ref = vec![0u8; num_px * 4];

        bgrx_to_rgba(&src, &mut dst_simd);
        bgrx_to_rgba_scalar(&src, &mut dst_ref);
        assert_eq!(dst_simd, dst_ref, "Mismatch for pixel count {num_px}");
    }
}

#[test]
fn test_simd_bgra_to_nv12_bt709_equivalence() {
    for (w, h) in [(16u32, 4u32), (34, 6), (1366, 4), (1920, 4)] {
        let stride = w * 4;
        let mut src = vec![0u8; (stride * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let off = (y * stride + x * 4) as usize;
                src[off] = ((x * 5 + y * 13) % 256) as u8;
                src[off + 1] = ((x * 11 + y * 3) % 256) as u8;
                src[off + 2] = ((x * 17 + y * 7) % 256) as u8;
                src[off + 3] = 255;
            }
        }

        let mut y_simd = vec![0u8; (w * h) as usize];
        let mut uv_simd = vec![0u8; (w * h / 2) as usize];
        let mut y_ref = vec![0u8; (w * h) as usize];
        let mut uv_ref = vec![0u8; (w * h / 2) as usize];

        bgra_to_nv12_bt709(&src, w, h, stride, &mut y_simd, w, &mut uv_simd, w);
        bgra_to_nv12_bt709_scalar(&src, w as usize, h as usize, stride as usize, &mut y_ref, w as usize, &mut uv_ref, w as usize);

        assert_eq!(y_simd, y_ref, "Y plane mismatch for {w}x{h}");
        assert_eq!(uv_simd, uv_ref, "UV plane mismatch for {w}x{h}");
    }
}

#[test]
fn test_ipc_binary_and_legacy_text_framing() {
    let (mut client, mut server) = UnixStream::pair().unwrap();
    let hdr = encode_frame_header(IpcOpCode::Freeze, 0);
    client.write_all(&hdr).unwrap();
    let (op, payload) = read_incoming_command(&mut server).unwrap();
    assert_eq!(op, IpcOpCode::Freeze);
    assert!(payload.is_empty());

    let (mut client2, mut server2) = UnixStream::pair().unwrap();
    client2.write_all(b"stop_recording\n").unwrap();
    let (op2, _) = read_incoming_command(&mut server2).unwrap();
    assert_eq!(op2, IpcOpCode::StopRecording);
}

#[test]
fn test_shm_buffer_rw_and_fd_dup() {
    let mut shm = ShmBuffer::new("vexrec-test", 8192).unwrap();
    shm.as_mut_slice()[0..4].copy_from_slice(&[10, 20, 30, 40]);
    assert_eq!(&shm.as_slice()[0..4], &[10, 20, 30, 40]);
    let dup = shm.try_clone_fd().unwrap();
    let mut f = std::fs::File::from(dup);
    let mut buf = [0u8; 4];
    f.read_exact(&mut buf).unwrap();
    assert_eq!(buf, [10, 20, 30, 40]);
}

#[test]
fn test_font_engine_and_drm_sysfs_probe() {
    let fonts = FontEngine::load_system_fonts();
    let (w, h) = fonts.measure_line(FontRole::Sans, 13.0, "Selection (1)");
    assert!(w > 0 && h > 0);

    let _devs = drm::enumerate_drm_devices();
}
