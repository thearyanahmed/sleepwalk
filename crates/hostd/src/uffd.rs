//! The userfaultfd page-fault server — lazy restore (objective O2).
//!
//! On restore, the target host does **not** read the whole guest RAM back in
//! before resuming. Instead the guest memory region is registered with
//! [`userfaultfd(2)`], the VM is resumed immediately, and each page is faulted
//! in on first touch: the kernel traps the access, this server reads the page
//! from the snapshot memory file and hands it to the kernel with `UFFDIO_COPY`,
//! and the guest thread continues. The freeze window shrinks from "copy all of
//! RAM" to "copy nothing"; pages arrive on demand.
//!
//! This module owns the only `unsafe` in hostd. The shape mirrors Firecracker's
//! reference UFFD handler: one dedicated thread blocks on the uffd, serving
//! faults as they arrive (page-fault latency is on the guest's critical path, so
//! it gets its own OS thread by design). The memory region and the snapshot file
//! are supplied from the edges — the caller (the restore path, or a test) maps
//! the region and opens the file; the logic here is just "fault in → which page
//! → copy or zero".
//!
//! [`userfaultfd(2)`]: https://man7.org/linux/man-pages/man2/userfaultfd.2.html

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, Ordering};

use thiserror::Error;
use userfaultfd::{Event, Uffd, UffdBuilder};

/// The OS page size (the granularity of every fault and copy).
#[must_use]
pub fn page_size() -> usize {
    // SAFETY: sysconf with a valid name has no preconditions and no side effects.
    let sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if sz > 0 { sz as usize } else { 4096 }
}

/// Something that can supply the bytes for a faulted page.
///
/// `offset` is the byte offset of the page within the guest region (always
/// page-aligned). Implementations fill `page` (exactly one page) and return
/// `true`, or return `false` to signal a hole — a page with no backing content,
/// which the server maps as zeros.
pub trait PageSource: Send {
    /// Fill `page` with the content for `offset`, or return `false` for a hole.
    ///
    /// # Errors
    /// Any I/O error reading the backing store.
    fn fill(&self, offset: u64, page: &mut [u8]) -> io::Result<bool>;
}

/// A [`PageSource`] backed by a snapshot memory file, read with positioned reads
/// (`pread`) so no shared cursor is contended across faults.
#[derive(Debug)]
pub struct FilePageSource {
    file: File,
    len: u64,
}

impl FilePageSource {
    /// Open a snapshot memory file as a page source.
    ///
    /// # Errors
    /// If the file cannot be opened or its length cannot be read.
    pub fn open(path: impl AsRef<std::path::Path>) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Self { file, len })
    }
}

impl PageSource for FilePageSource {
    fn fill(&self, offset: u64, page: &mut [u8]) -> io::Result<bool> {
        if offset >= self.len {
            return Ok(false); // past the end of the snapshot — a hole
        }
        let n = self.file.read_at(page, offset)?;
        if n == 0 {
            return Ok(false);
        }
        if n < page.len() {
            // A short final page: zero-fill the tail so the guest never sees
            // stale bytes from this buffer.
            page[n..].fill(0);
        }
        Ok(true)
    }
}

/// An error from the page-fault server.
#[derive(Debug, Error)]
pub enum UffdError {
    /// A userfaultfd operation failed.
    #[error("userfaultfd: {0}")]
    Uffd(#[from] userfaultfd::Error),
    /// An I/O error reading the page source.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A fault landed outside the registered region — a logic error, never the
    /// guest's fault, so it is surfaced rather than served.
    #[error("fault at {addr:#x} outside region [{base:#x}, {end:#x})")]
    OutOfRange {
        /// The faulting address.
        addr: usize,
        /// The region base.
        base: usize,
        /// One past the region end.
        end: usize,
    },
}

/// Serves missing-page faults for one registered guest memory region.
pub struct PageFaultServer {
    uffd: Uffd,
    base: usize,
    len: usize,
    page: usize,
    source: Box<dyn PageSource>,
}

impl PageFaultServer {
    /// Register `[base, base + len)` with `uffd` for missing-page faults and
    /// serve them from `source`.
    ///
    /// The region must already be mapped and page-aligned, and `uffd` freshly
    /// created. This does not resume the VM — the caller does that *after*
    /// registration, never before, or the first faults are lost.
    ///
    /// # Errors
    /// If the kernel rejects the registration.
    pub fn register(
        uffd: Uffd,
        base: usize,
        len: usize,
        source: Box<dyn PageSource>,
    ) -> Result<Self, UffdError> {
        // SAFETY: `base..base+len` is a mapped region supplied by the caller;
        // register only asks the kernel to trap faults on it, it does not deref.
        uffd.register(base as *mut c_void, len)?;
        Ok(Self {
            uffd,
            base,
            len,
            page: page_size(),
            source,
        })
    }

    /// Serve faults until `stop` is set.
    ///
    /// The uffd is non-blocking, so `read_event` returns `None` rather than
    /// blocking when no event is queued. `poll` with a short timeout keeps the
    /// loop off the CPU between faults while still re-checking `stop` promptly —
    /// no final fault is needed to unwedge a blocking read.
    ///
    /// # Errors
    /// If polling, reading an event, reading the source, or a copy/zero op fails.
    pub fn serve(&self, stop: &AtomicBool) -> Result<(), UffdError> {
        let mut pfd = libc::pollfd {
            fd: self.uffd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        while !stop.load(Ordering::Acquire) {
            // SAFETY: pfd points to one valid pollfd; poll only reads/writes it.
            let ready = unsafe { libc::poll(&mut pfd, 1, 200) };
            if ready < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err.into());
            }
            if ready == 0 {
                continue; // timeout — re-check the stop flag
            }
            // Non-blocking: drain every event poll signalled, then loop.
            while let Some(event) = self.uffd.read_event()? {
                if let Event::Pagefault { addr, .. } = event {
                    self.serve_page(addr as usize)?;
                }
                // fork/remap/remove notifications need nothing here.
            }
        }
        Ok(())
    }

    /// Resolve one fault: copy the backing page in, or map a zero page for a hole.
    fn serve_page(&self, addr: usize) -> Result<(), UffdError> {
        let aligned = addr & !(self.page - 1);
        let end = self.base + self.len;
        if aligned < self.base || aligned >= end {
            return Err(UffdError::OutOfRange {
                addr,
                base: self.base,
                end,
            });
        }
        let offset = (aligned - self.base) as u64;
        let mut buf = vec![0u8; self.page];
        if self.source.fill(offset, &mut buf)? {
            // SAFETY: `aligned` is page-aligned and within the registered region;
            // `buf` is exactly one page. copy hands these bytes to the kernel,
            // which writes them into the faulting page and wakes the waiter.
            unsafe {
                self.uffd
                    .copy(buf.as_ptr().cast(), aligned as *mut c_void, self.page, true)?;
            }
        } else {
            // SAFETY: same region/alignment invariants; zeropage installs a zero
            // page at the faulting address and wakes the waiter.
            unsafe {
                self.uffd
                    .zeropage(aligned as *mut c_void, self.page, true)?;
            }
        }
        Ok(())
    }
}

/// Build a userfaultfd suitable for serving guest-memory faults.
///
/// # Errors
/// If the kernel refuses to create the uffd (e.g. `vm.unprivileged_userfaultfd`
/// is disabled and the caller is unprivileged).
pub fn create_uffd() -> Result<Uffd, UffdError> {
    Ok(UffdBuilder::new()
        .close_on_exec(true)
        .non_blocking(true)
        .user_mode_only(true)
        .create()?)
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::slice;
    use std::sync::Arc;
    use std::thread;

    use super::*;

    /// Map an anonymous private region to stand in for guest RAM. Returns the
    /// base address; the caller munmaps it.
    fn map_region(len: usize) -> usize {
        // SAFETY: a standard anonymous mmap; on success the kernel returns a
        // valid `len`-byte mapping, which we own until munmap.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(p, libc::MAP_FAILED, "mmap failed");
        p as usize
    }

    fn unmap(base: usize, len: usize) {
        // SAFETY: base/len came from map_region and are not used afterwards.
        unsafe {
            libc::munmap(base as *mut c_void, len);
        }
    }

    /// Pages are faulted in lazily from the backing file, and a region that runs
    /// past the end of the snapshot file reads back as zeros (a hole).
    #[test]
    fn serves_pages_lazily_and_zeros_holes() {
        let page = page_size();
        let region_pages = 4usize;
        let len = page * region_pages;

        // Snapshot file backs only the first three pages, each filled with a
        // distinct byte; the fourth page has no backing => hole => zeros.
        let mut tmp = tempfile_in_cwd();
        for p in 0u8..3 {
            tmp.write_all(&vec![p + 1; page]).expect("write page");
        }
        tmp.flush().expect("flush");
        let source = Box::new(FilePageSource::open(&tmp.path).expect("open source"));

        let base = map_region(len);
        let uffd = create_uffd().expect("create uffd");
        let server = PageFaultServer::register(uffd, base, len, source).expect("register");

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let server_thread = thread::spawn(move || server.serve(&stop_thread));

        // Touch every page in a worker so the main thread can bound the wait: a
        // fault the server never resolves would otherwise hang forever. Each
        // first read triggers a fault the server resolves from the backing file.
        let (tx, rx) = std::sync::mpsc::channel();
        let toucher = thread::spawn(move || {
            // SAFETY: [base, base+len) is registered and served by server_thread.
            let mem = unsafe { slice::from_raw_parts(base as *const u8, len) };
            let mut seen = Vec::with_capacity(region_pages);
            for i in 0..region_pages {
                let off = i * page;
                seen.push((mem[off], mem[off + page - 1]));
            }
            let _ = tx.send(seen);
        });

        let seen = match rx.recv_timeout(std::time::Duration::from_secs(15)) {
            Ok(seen) => seen,
            Err(_) => {
                stop.store(true, Ordering::Release);
                panic!("page-fault server deadlocked: pages not faulted in within 15s");
            }
        };

        // Pages 0-2 carry their backing byte (first and last byte of the page);
        // page 3 is a hole, served as zeros.
        assert_eq!(seen, vec![(1, 1), (2, 2), (3, 3), (0, 0)]);

        stop.store(true, Ordering::Release);
        toucher.join().expect("toucher thread");
        server_thread
            .join()
            .expect("server thread")
            .expect("serve loop");

        unmap(base, len);
    }

    // Minimal temp-file helpers (no external dev-dep): a uniquely-named file in
    // the target dir, removed on drop.
    struct TmpFile {
        file: File,
        path: std::path::PathBuf,
    }
    impl std::ops::Deref for TmpFile {
        type Target = File;
        fn deref(&self) -> &File {
            &self.file
        }
    }
    impl Write for TmpFile {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.file.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.file.flush()
        }
    }
    impl Drop for TmpFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
    fn tempfile_in_cwd() -> TmpFile {
        // SAFETY: getpid is always safe.
        let pid = unsafe { libc::getpid() };
        let path = std::env::temp_dir().join(format!("sleepwalk-uffd-{pid}.mem"));
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("create temp file");
        TmpFile { file, path }
    }
}
