// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::filesystem::{DirEntry, DirectoryIterator};

use std::ffi::CStr;
use std::io;
use std::mem::size_of;
use std::ops::{Deref, DerefMut};
use std::os::unix::io::AsRawFd;

use log::debug;

use vm_memory::ByteValued;

#[repr(C, packed)]
#[derive(Default, Clone, Copy)]
struct LinuxDirent64 {
    d_ino: libc::ino64_t,
    d_off: libc::off64_t,
    d_reclen: libc::c_ushort,
    d_ty: libc::c_uchar,
}
unsafe impl ByteValued for LinuxDirent64 {}

#[derive(Default)]
pub struct ReadDir<P> {
    buf: P,
    current: usize,
    end: usize,
}

impl<P: DerefMut<Target = [u8]>> ReadDir<P> {
    /// Seek to `offset` in the directory and read a batch of entries.
    ///
    /// For offsets that exceed `i64::MAX` (e.g., AWS EFS cookies with the
    /// high bit set), the `u64`-to-`off64_t` cast produces a negative value,
    /// which the kernel rejects. In that case we fall back to scan from the
    /// beginning and move forward to the target cookie.
    pub fn new<D: AsRawFd>(dir: &D, offset: u64, buf: P) -> io::Result<Self> {
        // SAFETY: Safe because this doesn't modify any memory and we check the return value.
        // The possible wrapping of the `u64` to `i64` cast is intended.
        let res =
            unsafe { libc::lseek64(dir.as_raw_fd(), offset as libc::off64_t, libc::SEEK_SET) };
        if res >= 0 {
            return unsafe { Self::new_no_seek(dir, buf) };
        }

        // Only fall back for `EINVAL` caused by a large offset.
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINVAL) || offset <= i64::MAX as u64 {
            return Err(err);
        }

        debug!("lseek64 failed for directory offset {offset:#x}, falling back to linear scan");

        Self::new_walk_to(dir, offset, buf)
    }

    /// Fallback for offsets that `lseek64` cannot handle: rewinds and walks
    /// entries until the target cookie.
    fn new_walk_to<D: AsRawFd>(dir: &D, target_offset: u64, mut buf: P) -> io::Result<Self> {
        // SAFETY: Safe because this doesn't modify any memory and we check the return value.
        let res = unsafe { libc::lseek64(dir.as_raw_fd(), 0, libc::SEEK_SET) };
        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        loop {
            let mut rd = unsafe { Self::new_no_seek(dir, buf)? };
            if rd.remaining() == 0 {
                return Ok(rd); // not found
            }

            // `d_off` is an opaque cookie interpretable only by the filesystem
            // that generated it. It may be supplied as an offset to `lseek()`
            // to position the directory stream after the current entry (not at it).
            // So to resume from a given cookie, we must skip past the entry with
            // `d_off == target_offset`, and return everything that follows.
            if rd.move_past(target_offset) {
                if rd.remaining() > 0 {
                    return Ok(rd); // found
                }
                // it's last in this batch, fd is already at the next one
                return unsafe { Self::new_no_seek(dir, rd.buf) };
            }

            buf = rd.buf; // not in this batch
        }
    }

    /// Move past `target_offset`. Everything remaining is what the guest has not yet received.
    fn move_past(&mut self, target_offset: u64) -> bool {
        while let Some(entry) = DirectoryIterator::next(self) {
            if entry.offset == target_offset {
                return true;
            }
        }
        false
    }

    /// Continue reading from the current position in the directory without seeking.
    ///
    /// # Safety
    /// Caller must ensure the current position is valid, for example, by exclusively using this
    /// function on a given FD, potentially repeatedly.
    pub unsafe fn new_no_seek<D: AsRawFd>(dir: &D, mut buf: P) -> io::Result<Self> {
        // Safe because the kernel guarantees that it will only write to `buf` and we check the
        // return value.
        let res = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                dir.as_raw_fd(),
                buf.as_mut_ptr() as *mut LinuxDirent64,
                buf.len() as libc::c_int,
            )
        };
        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(ReadDir {
            buf,
            current: 0,
            end: res as usize,
        })
    }
}

impl<P> ReadDir<P> {
    /// Returns the number of bytes from the internal buffer that have not yet been consumed.
    pub fn remaining(&self) -> usize {
        self.end.saturating_sub(self.current)
    }
}

impl<P: Deref<Target = [u8]>> DirectoryIterator for ReadDir<P> {
    fn next(&mut self) -> Option<DirEntry<'_>> {
        let rem = &self.buf[self.current..self.end];
        if rem.is_empty() {
            return None;
        }

        // We only use debug asserts here because these values are coming from the kernel and we
        // trust them implicitly.
        debug_assert!(
            rem.len() >= size_of::<LinuxDirent64>(),
            "not enough space left in `rem`"
        );

        let (front, back) = rem.split_at(size_of::<LinuxDirent64>());

        let dirent64 =
            LinuxDirent64::from_slice(front).expect("unable to get LinuxDirent64 from slice");

        let namelen = dirent64.d_reclen as usize - size_of::<LinuxDirent64>();
        debug_assert!(namelen <= back.len(), "back is smaller than `namelen`");

        // The kernel will pad the name with additional nul bytes until it is 8-byte aligned so
        // we need to strip those off here.
        let name = strip_padding(&back[..namelen]);
        let entry = DirEntry {
            ino: dirent64.d_ino,
            offset: dirent64.d_off as u64,
            type_: dirent64.d_ty as u32,
            name,
        };

        debug_assert!(
            rem.len() >= dirent64.d_reclen as usize,
            "rem is smaller than `d_reclen`"
        );
        self.current += dirent64.d_reclen as usize;

        Some(entry)
    }
}

// Like `CStr::from_bytes_with_nul` but strips any bytes after the first '\0'-byte. Panics if `b`
// doesn't contain any '\0' bytes.
fn strip_padding(b: &[u8]) -> &CStr {
    // It would be nice if we could use memchr here but that's locked behind an unstable gate.
    let pos = b
        .iter()
        .position(|&c| c == 0)
        .expect("`b` doesn't contain any nul bytes");

    // Safe because we are creating this string with the first nul-byte we found so we can
    // guarantee that it is nul-terminated and doesn't contain any interior nuls.
    unsafe { CStr::from_bytes_with_nul_unchecked(&b[..=pos]) }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::ffi::CString;
    use std::fs;
    use vmm_sys_util::tempdir::TempDir;

    fn make_test_dir(num_files: usize) -> (TempDir, fs::File) {
        let td = TempDir::new_with_prefix("/tmp/virtiofsd_test_").unwrap();
        for i in 0..num_files {
            fs::write(td.as_path().join(format!("file_{i:03}")), b"").unwrap();
        }
        let file = fs::File::open(td.as_path()).unwrap();
        (td, file)
    }

    /// Build a fake `ReadDir` from raw dirent fields.
    /// `entry` is `(d_ino, d_off, name)`.
    fn build_dirent_buf(entries: &[(u64, u64, &[u8])]) -> ReadDir<Vec<u8>> {
        let header = size_of::<LinuxDirent64>();
        let mut buf = vec![0u8; 4096];
        let mut pos = 0;
        for &(ino, d_off, name) in entries {
            let reclen = (header + name.len() + 1).next_multiple_of(8);
            let dirent = LinuxDirent64 {
                d_ino: ino,
                d_off: d_off as i64,
                d_reclen: reclen as libc::c_ushort,
                d_ty: libc::DT_REG,
            };
            assert!(
                pos + reclen <= buf.len(),
                "dirent buffer overflow: need {} bytes but buf is {}",
                pos + reclen,
                buf.len()
            );
            buf[pos..pos + header].copy_from_slice(dirent.as_slice());
            buf[pos + header..pos + header + name.len()].copy_from_slice(name);
            pos += reclen;
        }
        ReadDir {
            buf,
            current: 0,
            end: pos,
        }
    }

    fn next_entry(rd: &mut ReadDir<Vec<u8>>) -> Option<(u64, u64, CString)> {
        let e = rd.next()?;
        Some((e.ino, e.offset, e.name.to_owned()))
    }

    /// Read all entries from `directory`. Returns `(d_ino, d_off, name)` tuples.
    fn collect_entries(directory: &fs::File) -> Vec<(u64, u64, CString)> {
        let mut entries = Vec::new();
        let mut offset: u64 = 0;
        loop {
            let mut rd = ReadDir::new(directory, offset, vec![0u8; 4096]).unwrap();
            if rd.remaining() == 0 {
                break;
            }
            while let Some(e) = next_entry(&mut rd) {
                offset = e.1;
                entries.push(e);
            }
        }
        entries
    }

    #[test]
    fn padded_cstrings() {
        assert_eq!(strip_padding(b".\0\0\0\0\0\0\0").to_bytes(), b".");
        assert_eq!(strip_padding(b"..\0\0\0\0\0\0").to_bytes(), b"..");
        assert_eq!(
            strip_padding(b"normal cstring\0").to_bytes(),
            b"normal cstring"
        );
        assert_eq!(strip_padding(b"\0\0\0\0").to_bytes(), b"");
        assert_eq!(
            strip_padding(b"interior\0nul bytes\0\0\0").to_bytes(),
            b"interior"
        );
    }

    #[test]
    #[should_panic(expected = "`b` doesn't contain any nul bytes")]
    fn no_nul_byte() {
        strip_padding(b"no nul bytes in string");
    }

    #[test]
    fn move_past_finds_target_in_middle() {
        let mut rd = build_dirent_buf(&[(1, 100, b"a"), (2, 200, b"b"), (3, 300, b"c")]);
        assert!(ReadDir::move_past(&mut rd, 200));
        let e = next_entry(&mut rd).unwrap();
        assert_eq!((e.0, e.1), (3, 300));
    }

    #[test]
    fn move_past_finds_target_at_end() {
        let mut rd = build_dirent_buf(&[(1, 100, b"a"), (2, 200, b"b")]);
        assert!(ReadDir::move_past(&mut rd, 200));
        assert_eq!(rd.remaining(), 0);
    }

    #[test]
    fn move_past_not_found() {
        let mut rd = build_dirent_buf(&[(1, 100, b"a"), (2, 200, b"b")]);
        assert!(!ReadDir::move_past(&mut rd, 999));
        assert_eq!(rd.remaining(), 0);
    }

    #[test]
    fn move_past_large_cookie() {
        let large = u64::MAX - 42;
        let mut rd = build_dirent_buf(&[(1, 100, b"a"), (2, large, b"b"), (3, 300, b"c")]);
        assert!(ReadDir::move_past(&mut rd, large));
        let e = next_entry(&mut rd).unwrap();
        assert_eq!((e.0, e.1), (3, 300));
    }

    #[test]
    fn new_walk_to_matches_lseek_path() {
        let (_td, file) = make_test_dir(10);
        let all = collect_entries(&file);

        for i in 0..all.len() - 1 {
            let cookie = all[i].1;
            let mut rd_seek = ReadDir::new(&file, cookie, vec![0u8; 4096]).unwrap();
            let mut rd_walk = ReadDir::new_walk_to(&file, cookie, vec![0u8; 4096]).unwrap();

            loop {
                match (next_entry(&mut rd_seek), next_entry(&mut rd_walk)) {
                    (None, None) => break,
                    (Some(a), Some(b)) => assert_eq!(a, b, "mismatch at cookie {}", cookie),
                    _ => panic!("entry count mismatch at cookie {}", cookie),
                }
            }
        }
    }

    #[test]
    fn new_walk_to_small_buffer() {
        let (_td, file) = make_test_dir(20);
        let all = collect_entries(&file);

        for i in 0..all.len() - 1 {
            let cookie = all[i].1;
            let mut rd = ReadDir::new_walk_to(&file, cookie, vec![0u8; 128]).unwrap();
            let e = next_entry(&mut rd).expect("expected entry after cookie");
            assert_eq!(e.0, all[i + 1].0, "ino mismatch at cookie {}", cookie);
        }
    }

    #[test]
    fn new_falls_back_to_walk_for_large_offset() {
        let (_td, file) = make_test_dir(1);
        let rd = ReadDir::new(&file, u64::MAX, vec![0u8; 4096]).unwrap();
        assert_eq!(rd.remaining(), 0);
    }
}
