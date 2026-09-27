use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::ptr::NonNull;

#[derive(Debug)]
pub(crate) struct MemoryMapping {
    addr: NonNull<u8>,
    len: usize,
}

impl MemoryMapping {
    pub(crate) fn anonymous(len: usize) -> io::Result<Self> {
        Self::map(-1, len, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS)
    }

    pub(crate) fn shared(fd: BorrowedFd<'_>, len: usize) -> io::Result<Self> {
        Self::map(fd.as_raw_fd(), len, libc::MAP_SHARED)
    }

    fn map(fd: RawFd, len: usize, flags: libc::c_int) -> io::Result<Self> {
        // SAFETY: Both callers let the kernel choose a fresh mapping address.
        // The fd is either borrowed for this call or unused for anonymous RAM.
        // No existing mapping is replaced, and no Rust references are created.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                fd,
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        // mmap can theoretically succeed at address zero. Reject that mapping
        // so this owner always provides a non-null pointer for Rust accesses.
        let Some(addr) = NonNull::new(addr.cast::<u8>()) else {
            // SAFETY: mmap succeeded, and this mapping has not been exposed.
            unsafe { libc::munmap(addr, len) };
            return Err(io::Error::other("mmap returned address zero"));
        };

        Ok(Self { addr, len })
    }

    pub(crate) fn as_ptr(&self) -> *mut u8 {
        self.addr.as_ptr()
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

impl Drop for MemoryMapping {
    fn drop(&mut self) {
        // SAFETY: This owner uniquely owns the original mapping. Its address
        // and length are unchanged, and callers must stop using it before drop.
        unsafe { libc::munmap(self.addr.as_ptr().cast(), self.len) };
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{remove_file, OpenOptions};
    use std::os::fd::AsFd;
    use std::os::unix::fs::FileExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::MemoryMapping;

    #[test]
    fn mapping_is_zeroed_and_writable_at_both_ends() {
        let memory = MemoryMapping::anonymous(2 * 1024 * 1024).unwrap();

        // SAFETY: Both offsets are inside the live mapping. No VM, other
        // thread, or Rust reference can access this memory during the test.
        unsafe {
            let first = memory.as_ptr();
            let last = first.add(memory.len() - 1);
            assert_eq!(first.read(), 0);
            assert_eq!(last.read(), 0);
            first.write(0x5a);
            last.write(0xa5);
            assert_eq!(first.read(), 0x5a);
            assert_eq!(last.read(), 0xa5);
        }
    }

    #[test]
    fn empty_mapping_preserves_mmap_error() {
        let error = MemoryMapping::anonymous(0).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn shared_mapping_observes_file_writes_and_updates_the_file() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("kvm-mapping-test-{}-{nonce}", std::process::id(),));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        remove_file(path).unwrap();
        file.set_len(4096).unwrap();
        let mapping = MemoryMapping::shared(file.as_fd(), 4096).unwrap();

        file.write_all_at(&[0x5a], 17).unwrap();
        // SAFETY: Offset 17 is within the live mapping and file. All file and
        // mapping accesses are sequential, with no other owners or references.
        unsafe {
            let byte = mapping.as_ptr().add(17);
            assert_eq!(byte.read_volatile(), 0x5a);
            byte.write_volatile(0xa5);
        }
        let mut byte = [0];
        file.read_exact_at(&mut byte, 17).unwrap();
        assert_eq!(byte, [0xa5]);
    }
}
