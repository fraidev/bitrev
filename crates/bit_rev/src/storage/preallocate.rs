use std::fs::File;
use std::io;

/// Grow `file` to `len` with a real allocation. Falls back to sparse `set_len`
/// when the platform call is missing or fails.
pub fn allocate_full(file: &File, len: u64) -> io::Result<()> {
    match allocate_full_inner(file, len) {
        Ok(()) => Ok(()),
        Err(_) => file.set_len(len),
    }
}

#[cfg(target_os = "linux")]
fn allocate_full_inner(file: &File, len: u64) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, len as libc::off_t) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(rc))
    }
}

#[cfg(target_os = "macos")]
fn allocate_full_inner(file: &File, len: u64) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let mut store = libc::fstore_t {
        fst_flags: libc::F_ALLOCATEALL,
        fst_posmode: libc::F_PEOFPOSMODE,
        fst_offset: 0,
        fst_length: len as libc::off_t,
        fst_bytesalloc: 0,
    };
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &mut store) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    file.set_len(len)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn allocate_full_inner(file: &File, len: u64) -> io::Result<()> {
    file.set_len(len)
}
