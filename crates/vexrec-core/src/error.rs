use thiserror::Error;

#[derive(Error, Debug)]
pub enum VexrecError {
    #[error("Display backend error: {0}")]
    Display(String),

    #[error("Renderer error: {0}")]
    Render(String),

    #[error("D-Bus / Portal error: {0}")]
    Dbus(String),

    #[error("Portal error: {0}")]
    Portal(String),

    #[error("PipeWire error: {0}")]
    PipeWire(String),

    #[error("Capture pipeline error: {0}")]
    Pipeline(String),

    #[error("Encoding error: {0}")]
    Encoding(String),

    #[error("Muxer error: {0}")]
    Muxer(String),

    #[error("Audio device error: {0}")]
    Audio(String),

    #[error("Camera device error: {0}")]
    Camera(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Session is not active or already in invalid state: {0}")]
    InvalidSessionState(String),
}

pub type LuxrecError = VexrecError;
pub type Result<T> = std::result::Result<T, VexrecError>;
