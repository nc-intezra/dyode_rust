//! Wake up when something changes under a folder (inotify), with a plain
//! timed wait as the fallback.  The callers rescan the folder either way,
//! so a missed event only costs latency.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct ChangeWatcher {
    root: PathBuf,
    recursive: bool,
    fd: Option<libc::c_int>,
}

const MASK: u32 = libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_TO
    | libc::IN_CREATE
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF;

impl ChangeWatcher {
    pub fn new(root: &Path, recursive: bool) -> Self {
        // SAFETY: plain syscall.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        let fd = if fd < 0 {
            log::warn!("inotify unavailable: polling {} instead", root.display());
            None
        } else {
            Some(fd)
        };
        let w = Self {
            root: root.to_path_buf(),
            recursive,
            fd,
        };
        w.add_all();
        w
    }

    fn add(&self, dir: &Path) {
        let Some(fd) = self.fd else { return };
        let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else {
            return;
        };
        // SAFETY: valid fd and path.
        if unsafe { libc::inotify_add_watch(fd, c.as_ptr(), MASK | libc::IN_ONLYDIR) } < 0 {
            log::debug!(
                "cannot watch {}: {}",
                dir.display(),
                std::io::Error::last_os_error()
            );
        }
    }

    fn add_all(&self) {
        self.add(&self.root);
        if !self.recursive {
            return;
        }
        let mut stack = vec![self.root.clone()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() && !e.file_name().to_string_lossy().starts_with(".dyode") {
                    self.add(&e.path());
                    stack.push(e.path());
                }
            }
        }
    }

    /// Block up to `timeout`.  Returns true if events arrived.
    pub fn wait(&self, timeout: Duration) -> bool {
        let Some(fd) = self.fd else {
            std::thread::sleep(timeout);
            return false;
        };
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: one valid pollfd.
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc <= 0 {
            return false;
        }
        let mut buf = [0u8; 8192];
        let mut saw_dir = false;
        let mut any = false;
        loop {
            // SAFETY: buf is valid for its length.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            any = true;
            let mut off = 0usize;
            let hdr = std::mem::size_of::<libc::inotify_event>();
            while off + hdr <= n as usize {
                // SAFETY: the kernel wrote complete events into buf.
                let ev: libc::inotify_event =
                    unsafe { std::ptr::read_unaligned(buf[off..].as_ptr().cast()) };
                if ev.mask & libc::IN_ISDIR != 0 {
                    saw_dir = true;
                }
                off += hdr + ev.len as usize;
            }
        }
        if saw_dir && self.recursive {
            self.add_all(); // watch newly created subfolders
        }
        any
    }
}

impl Drop for ChangeWatcher {
    fn drop(&mut self) {
        if let Some(fd) = self.fd {
            // SAFETY: we own fd.
            unsafe { libc::close(fd) };
        }
    }
}
