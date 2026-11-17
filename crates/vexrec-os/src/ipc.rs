use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;
use vexrec_core::error::{Result, VexrecError};
use vexrec_core::xdg;

pub const BINARY_MAGIC: [u8; 2] = *b"VX";
pub const BINARY_VERSION: u8 = 1;
pub const LEGACY_TMP_SOCK: &str = "/tmp/vexrec.sock";
pub const LEGACY_LUX_SOCK: &str = "/tmp/luxrec.sock";

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcOpCode {
    Freeze = 1,
    CancelFreeze = 2,
    StopRecording = 3,
    TogglePause = 4,
    Ping = 5,
    CustomText = 6,
}

impl IpcOpCode {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Freeze),
            2 => Some(Self::CancelFreeze),
            3 => Some(Self::StopRecording),
            4 => Some(Self::TogglePause),
            5 => Some(Self::Ping),
            6 => Some(Self::CustomText),
            _ => None,
        }
    }

    pub fn from_text(cmd: &str) -> Option<Self> {
        match cmd.trim() {
            "freeze" => Some(Self::Freeze),
            "cancel_freeze" => Some(Self::CancelFreeze),
            "stop_recording" => Some(Self::StopRecording),
            "toggle_pause" => Some(Self::TogglePause),
            "ping" => Some(Self::Ping),
            _ => None,
        }
    }

    pub const fn as_text(self) -> &'static str {
        match self {
            Self::Freeze => "freeze",
            Self::CancelFreeze => "cancel_freeze",
            Self::StopRecording => "stop_recording",
            Self::TogglePause => "toggle_pause",
            Self::Ping => "ping",
            Self::CustomText => "custom",
        }
    }
}

/// Primary runtime socket inside `$XDG_RUNTIME_DIR/vexrec.sock`.
pub fn primary_socket_path() -> PathBuf {
    xdg::runtime_dir().join("vexrec.sock")
}

/// Encodes an 8-byte zero-allocation binary header `[magic(2), ver(1), op(1), len(4)]`.
pub const fn encode_frame_header(op: IpcOpCode, payload_len: u32) -> [u8; 8] {
    let len_le = payload_len.to_le_bytes();
    [
        BINARY_MAGIC[0],
        BINARY_MAGIC[1],
        BINARY_VERSION,
        op as u8,
        len_le[0],
        len_le[1],
        len_le[2],
        len_le[3],
    ]
}

fn connect_first_available() -> std::io::Result<UnixStream> {
    let primary = primary_socket_path();
    if let Ok(s) = UnixStream::connect(&primary) {
        return Ok(s);
    }
    if let Ok(s) = UnixStream::connect(LEGACY_TMP_SOCK) {
        return Ok(s);
    }
    UnixStream::connect(LEGACY_LUX_SOCK)
}

/// Ultra-fast stack-allocated binary IPC trigger (< 0.3 ms round-trip).
pub fn send_fast_opcode(op: IpcOpCode, timeout: Duration) -> Result<String> {
    let mut stream = connect_first_available()
        .map_err(|e| VexrecError::Ipc(format!("Daemon socket unavailable: {e}")))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let hdr = encode_frame_header(op, 0);
    stream.write_all(&hdr)?;

    let mut resp = [0u8; 512];
    let n = stream.read(&mut resp).unwrap_or(0);
    let msg = String::from_utf8_lossy(&resp[..n]).trim().to_string();
    if let Some(err) = msg.strip_prefix("ERR:") {
        return Err(VexrecError::Ipc(err.trim().to_string()));
    }
    Ok(msg)
}

/// Reads either a binary V2 frame or legacy UTF-8 newline command from a client socket.
pub fn read_incoming_command(stream: &mut UnixStream) -> Result<(IpcOpCode, Vec<u8>)> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let first = reader.fill_buf()?;
    if first.len() >= 2 && first[0] == BINARY_MAGIC[0] && first[1] == BINARY_MAGIC[1] {
        let mut hdr = [0u8; 8];
        reader.read_exact(&mut hdr)?;
        if hdr[2] != BINARY_VERSION {
            return Err(VexrecError::Ipc(format!("Unsupported IPC version {}", hdr[2])));
        }
        let op = IpcOpCode::from_u8(hdr[3])
            .ok_or_else(|| VexrecError::Ipc(format!("Unknown IPC opcode {}", hdr[3])))?;
        let plen = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
        if plen > 64 * 1024 {
            return Err(VexrecError::Ipc("IPC payload exceeded 64 KiB cap".to_string()));
        }
        let mut payload = vec![0u8; plen];
        if plen > 0 {
            reader.read_exact(&mut payload)?;
        }
        return Ok((op, payload));
    }
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let trimmed = line.trim();
    if let Some(op) = IpcOpCode::from_text(trimmed) {
        Ok((op, Vec::new()))
    } else {
        Ok((IpcOpCode::CustomText, trimmed.as_bytes().to_vec()))
    }
}

/// Binds the primary `$XDG_RUNTIME_DIR/vexrec.sock` (`0600`) and compatibility symlinks.
pub fn bind_daemon_listener() -> Result<UnixListener> {
    let primary = primary_socket_path();
    if let Some(parent) = primary.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(&primary);

    let listener = UnixListener::bind(&primary)
        .map_err(|e| VexrecError::Ipc(format!("Failed to bind {}: {e}", primary.display())))?;
    let _ = std::fs::set_permissions(&primary, std::fs::Permissions::from_mode(0o600));

    for legacy in [LEGACY_TMP_SOCK, LEGACY_LUX_SOCK] {
        let lp = Path::new(legacy);
        if lp != primary {
            let _ = std::fs::remove_file(lp);
            let _ = std::os::unix::fs::symlink(&primary, lp);
        }
    }

    Ok(listener)
}
