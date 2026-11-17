use std::os::fd::OwnedFd;
use std::ptr::NonNull;
use rustix::fs::{memfd_create, ftruncate, MemfdFlags};
use rustix::mm::{mmap, munmap, MapFlags, ProtFlags};
use vexrec_core::error::{Result, VexrecError};

/// Zero-leak shared-memory segment backed by Linux `memfd_create` (`MAP_SHARED`).
/// Ideal for MIT-SHM 1.2 `attach_fd` and Wayland `wl_shm` pools.
pub struct ShmBuffer {
    fd: OwnedFd,
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: Exclusive owner of mapping unless explicitly shared via dup'd fd to display server.
unsafe impl Send for ShmBuffer {}
unsafe impl Sync for ShmBuffer {}

impl ShmBuffer {
    pub fn new(name: &str, len: usize) -> Result<Self> {
        let actual_len = len.max(4096);
        let fd = memfd_create(name, MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)
            .map_err(|e| VexrecError::Io(std::io::Error::from_raw_os_error(e.raw_os_error())))?;

        ftruncate(&fd, actual_len as u64)
            .map_err(|e| VexrecError::Io(std::io::Error::from_raw_os_error(e.raw_os_error())))?;

        // SAFETY: Mapping `actual_len` bytes from freshly sized memfd descriptor.
        let raw = unsafe {
            mmap(
                std::ptr::null_mut(),
                actual_len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                &fd,
                0,
            )
            .map_err(|e| VexrecError::Io(std::io::Error::from_raw_os_error(e.raw_os_error())))?
        };

        let ptr = NonNull::new(raw.cast::<u8>())
            .ok_or_else(|| VexrecError::Display("mmap returned null pointer".to_string()))?;

        Ok(Self {
            fd,
            ptr,
            len: actual_len,
        })
    }

    pub fn fd(&self) -> &OwnedFd {
        &self.fd
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr valid for `self.len` bytes.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: ptr valid for `self.len` bytes with exclusive `&mut self`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    pub fn try_clone_fd(&self) -> Result<OwnedFd> {
        self.fd.try_clone().map_err(VexrecError::Io)
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` and `self.len` come from `mmap`.
        unsafe {
            let _ = munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}
