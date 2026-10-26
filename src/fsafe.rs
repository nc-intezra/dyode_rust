//! File-system operations that never follow a symbolic link.
//!
//! Both boxes run as root and both touch folders other people can write to
//! (drop folders, file shares).  A link planted there must neither make the
//! input box send a file it was not given (D-04) nor make the output box
//! write a received file somewhere else (D-01).  Everything here walks
//! paths one component at a time with `openat(O_NOFOLLOW)`, relative to a
//! descriptor of the folder before it.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

fn cstr(s: &[u8]) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
}

fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let c = cstr(path.as_os_str().as_bytes())?;
    // SAFETY: c is a valid NUL-terminated string.
    let fd = check(unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    })?;
    // SAFETY: fd was just returned by open and is owned by nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn openat(dir: RawFd, name: &str, flags: libc::c_int, mode: libc::mode_t) -> io::Result<OwnedFd> {
    let c = cstr(name.as_bytes())?;
    // SAFETY: valid dirfd and NUL-terminated name.
    let fd = check(unsafe {
        libc::openat(
            dir,
            c.as_ptr(),
            flags | libc::O_CLOEXEC,
            libc::c_uint::from(mode),
        )
    })?;
    // SAFETY: fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: st is a valid out-pointer.
    check(unsafe { libc::fstat(fd, &mut st) })?;
    Ok(st)
}

fn is_reg(st: &libc::stat) -> bool {
    (st.st_mode & libc::S_IFMT) == libc::S_IFREG
}

fn is_dir(st: &libc::stat) -> bool {
    (st.st_mode & libc::S_IFMT) == libc::S_IFDIR
}

/// Split a relative path received from the other side (or built by our own
/// scan) into components, refusing anything that could escape `root`:
/// absolute paths, '..', '.', empty components, NUL, and our own
/// '.dyode*' bookkeeping folders.
pub fn safe_parts(rel: &str) -> Option<Vec<&str>> {
    if rel.is_empty() || rel.contains('\0') || rel.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = rel.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == ".." || p.starts_with(".dyode"))
    {
        return None;
    }
    Some(parts)
}

/// Open root/rel for reading without following any symlink, in any path
/// component.  Returns the file and its (size, mtime) if it is a regular
/// file.
///
/// The scan only looked at the path.  Hashing and sending this same open
/// descriptor means a file swapped for a link after the scan cannot make
/// DYODE send something else.
pub fn open_ready_file(root: &Path, rel: &str) -> Option<(File, u64, SystemTime)> {
    let parts = rel.split('/').collect::<Vec<_>>();
    let mut dir = open_dir(root).ok()?;
    for part in &parts[..parts.len() - 1] {
        dir = openat(
            dir.as_raw_fd(),
            part,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
        .ok()?;
    }
    // O_NONBLOCK so a FIFO swapped in cannot hang us on open.
    let fd = openat(
        dir.as_raw_fd(),
        parts[parts.len() - 1],
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    )
    .ok()?;
    let st = fstat(fd.as_raw_fd()).ok()?;
    if !is_reg(&st) {
        return None;
    }
    // SAFETY: valid fd; clear O_NONBLOCK for ordinary reads.
    unsafe {
        let fl = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if fl >= 0 {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, fl & !libc::O_NONBLOCK);
        }
    }
    let mtime = SystemTime::UNIX_EPOCH + Duration::new(st.st_mtime as u64, st.st_mtime_nsec as u32);
    Some((File::from(fd), st.st_size as u64, mtime))
}

/// Why a received file could not be placed.
#[derive(Debug)]
pub enum PlaceError {
    /// A folder on the way is a link or not a folder, or the target is a folder.
    Unsafe,
    Io(io::Error),
}

/// Move `staged` to out_dir/rel without following any symlink, creating
/// missing folders on the way.  `rename()` replaces a link at the final
/// name rather than following it.  `staged` must be on the same
/// filesystem, so the move is atomic.
pub fn place_file(out_dir: &Path, rel: &str, staged: &Path) -> Result<PathBuf, PlaceError> {
    let parts = safe_parts(rel).ok_or(PlaceError::Unsafe)?;
    let mut dir = open_dir(out_dir).map_err(PlaceError::Io)?;
    for part in &parts[..parts.len() - 1] {
        let c = cstr(part.as_bytes()).map_err(PlaceError::Io)?;
        // SAFETY: valid dirfd and name.
        let rc = unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o755) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EEXIST) {
                return Err(PlaceError::Io(e));
            }
        }
        dir = openat(
            dir.as_raw_fd(),
            part,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
        .map_err(|_| PlaceError::Unsafe)?;
    }
    let last = parts[parts.len() - 1];
    let c_last = cstr(last.as_bytes()).map_err(PlaceError::Io)?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointers.
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            c_last.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == 0 && is_dir(&st) {
        return Err(PlaceError::Unsafe);
    }
    let c_staged = cstr(staged.as_os_str().as_bytes()).map_err(PlaceError::Io)?;
    // SAFETY: valid pointers and dirfd.
    check(unsafe {
        libc::renameat(
            libc::AT_FDCWD,
            c_staged.as_ptr(),
            dir.as_raw_fd(),
            c_last.as_ptr(),
        )
    })
    .map_err(PlaceError::Io)?;
    let mut dest = out_dir.to_path_buf();
    for p in parts {
        dest.push(p);
    }
    Ok(dest)
}

/// Set a file's modification time through its descriptor.
pub fn set_mtime(file: &File, mtime: f64) {
    let ts = libc::timespec {
        tv_sec: mtime.trunc() as libc::time_t,
        tv_nsec: (mtime.fract() * 1e9) as _,
    };
    let now = libc::timespec {
        tv_sec: 0,
        tv_nsec: libc::UTIME_NOW,
    };
    let times = [now, ts];
    // SAFETY: valid fd and a two-element array.
    unsafe {
        libc::futimens(file.as_raw_fd(), times.as_ptr());
    }
}

/// Regular files under `root` whose last change is at least `settle`
/// seconds old (half-written files wait for the next pass).  Returns the
/// sorted (relpath, path) list and how many files are still changing.
/// Symlinks are skipped on purpose, and so are '.dyode*' folders.
pub fn scan_ready_files(root: &Path, settle: f64) -> (Vec<(String, PathBuf)>, usize) {
    let now = SystemTime::now();
    let mut ready = Vec::new();
    let mut waiting = 0;
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        let mut subdirs = Vec::new();
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(md) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if md.file_type().is_dir() {
                if !name.starts_with(".dyode") {
                    subdirs.push((e.path(), rel));
                }
                continue;
            }
            if !md.file_type().is_file() {
                continue;
            }
            let age = md
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .map_or(0.0, |d| d.as_secs_f64());
            if age < settle {
                waiting += 1;
                continue;
            }
            ready.push((rel, e.path()));
        }
        // Depth-first in name order.
        for s in subdirs.into_iter().rev() {
            stack.push(s);
        }
    }
    ready.sort();
    (ready, waiting)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn safe_parts_rules() {
        assert!(safe_parts("a/b.txt").is_some());
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "./a",
            ".dyode_sent/x",
            "a\0b",
        ] {
            assert!(safe_parts(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn open_ready_file_refuses_links() {
        let d = crate::testutil::TempDir::new();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        std::fs::write(d.path().join("sub/real"), b"data").unwrap();
        symlink("/etc/hostname", d.path().join("link")).unwrap();
        symlink(d.path().join("sub"), d.path().join("dirlink")).unwrap();
        assert!(open_ready_file(d.path(), "sub/real").is_some());
        assert!(open_ready_file(d.path(), "link").is_none());
        assert!(open_ready_file(d.path(), "dirlink/real").is_none());
    }

    #[test]
    fn place_file_refuses_link_folders() {
        let d = crate::testutil::TempDir::new();
        let out = d.path().join("out");
        let elsewhere = d.path().join("elsewhere");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        symlink(&elsewhere, out.join("evil")).unwrap();
        let staged = d.path().join("blob");
        std::fs::write(&staged, b"x").unwrap();
        assert!(matches!(
            place_file(&out, "evil/f", &staged),
            Err(PlaceError::Unsafe)
        ));
        assert!(!elsewhere.join("f").exists());
        let dest = place_file(&out, "new/dir/f", &staged).unwrap();
        assert_eq!(std::fs::read(dest).unwrap(), b"x");
    }

    #[test]
    fn scan_skips_links_and_bookkeeping() {
        let d = crate::testutil::TempDir::new();
        std::fs::create_dir_all(d.path().join("a/.dyode_sent")).unwrap();
        std::fs::write(d.path().join("a/f1"), b"1").unwrap();
        std::fs::write(d.path().join("a/.dyode_sent/old"), b"1").unwrap();
        std::fs::write(d.path().join("z"), b"1").unwrap();
        symlink("/etc/hostname", d.path().join("l")).unwrap();
        let (ready, waiting) = scan_ready_files(d.path(), 0.0);
        let rels: Vec<_> = ready.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(rels, vec!["a/f1", "z"]);
        assert_eq!(waiting, 0);
        let (_, waiting) = scan_ready_files(d.path(), 3600.0);
        assert_eq!(waiting, 2);
    }
}
