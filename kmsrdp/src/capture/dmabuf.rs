use std::fs;
use std::io;
use std::os::unix::io::{AsFd, BorrowedFd, RawFd};

use drm::Device;
use drm::control::Device as ControlDevice;

#[derive(Debug)]
pub struct Card(pub fs::File);

#[repr(C)]
struct DmaBufSync {
    flags: u64,
}

const DMA_BUF_SYNC_READ: u64 = 1 << 0;
const DMA_BUF_SYNC_START: u64 = 0 << 2;
const DMA_BUF_SYNC_END: u64 = 1 << 2;

// _IOW('b', 0, struct dma_buf_sync) on Linux = 0x40086200
const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x40086200;

pub fn dma_buf_sync(fd: RawFd, flags: u64) {
    let sync = DmaBufSync { flags };
    let rc = unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync) };
    if rc != 0 {
        tracing::debug!(
            errno = %std::io::Error::last_os_error(),
            "DMA_BUF_IOCTL_SYNC failed; CPU read may see a stale cache"
        );
    }
}

pub fn dma_buf_sync_start(fd: RawFd) {
    dma_buf_sync(fd, DMA_BUF_SYNC_READ | DMA_BUF_SYNC_START);
}

pub fn dma_buf_sync_end(fd: RawFd) {
    dma_buf_sync(fd, DMA_BUF_SYNC_READ | DMA_BUF_SYNC_END);
}

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Device for Card {}
impl ControlDevice for Card {}

impl Card {
    pub fn open_read_only(path: &str) -> io::Result<Self> {
        let file = fs::OpenOptions::new().read(true).open(path)?;
        Ok(Card(file))
    }
}
