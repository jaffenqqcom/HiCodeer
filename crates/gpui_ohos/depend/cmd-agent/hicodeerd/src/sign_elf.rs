//! Signing of freshly installed native binaries for the OHOS sandbox.
//!
//! Two facts drive this module:
//!
//! 1. The kernel refuses to `exec` an ELF that carries no code-signature
//!    section (`.codesign`), failing with EACCES. npm unpacks packages straight
//!    from tarballs, so every native binary a package ships arrives unsigned --
//!    a package postinstall that runs it (esbuild's version probe) dies, and npm
//!    rolls the whole install back.
//! 2. Only that postinstall needs the binary while the install is running, and
//!    nothing needs it again before the install command returns. So
//!    `exec::rewrite_npm_para` defers the install's lifecycle scripts
//!    (`--ignore-scripts`) and the tree is signed here: right after the install
//!    exits, before its exit status reaches the client. The caller replays those
//!    scripts afterwards, by which time whatever they run is signed.
//!
//! The signer is resolved from PATH -- its directory is scanned, not executed --
//! and no installation path is hard-coded. Signing is best-effort end to end: a
//! missing tool, an unreadable file, or a non-zero signer exit leaves the tree as
//! it was. It must never turn a successful command into a failure.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Stdio};

/// Signer program, resolved from PATH.
const SIGNER: &str = "binary-sign-tool";
/// Section a signed OHOS ELF carries; its presence means a re-sign is pointless.
const CODESIGN_SECTION: &str = ".codesign";
/// Sub-tree an install deposits packages into, under the install root.
const PACKAGE_DIR: &str = "node_modules";
/// Directory names never descended into: VCS metadata and npm's scratch cache.
const SKIP_DIRS: [&str; 2] = [".git", ".cache"];
/// Cap on files a single sweep will look at, so a pathological tree cannot stall
/// the command's exit report.
const MAX_FILES: usize = 20000;
/// Cap on how deep a sweep descends.
const MAX_DEPTH: usize = 12;
/// Suffix of the signer's output file, before it replaces the original.
const SIGNED_SUFFIX: &str = ".signed";
/// Mode applied to a signed binary: owner and group read/write/execute. The
/// signer writes its output group-writable but WITHOUT execute, and the in-place
/// move carries that over.
const SIGNED_MODE: u32 = 0o775;
/// ELF magic, and the `EI_CLASS`/`EI_DATA` values of the class and byte order
/// this module can walk (native aarch64 objects).
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS32: u8 = 1;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
/// Sanity bounds on the section table: enough for any real object, small enough
/// that a corrupt header cannot make us allocate wildly.
const MAX_SECTIONS: u64 = 4096;
const MAX_STRTAB: u64 = 1 << 20;

/// Signs every unsigned ELF under `root`'s package tree, returning how many
/// files were signed. Never fails; see the module note.
pub fn sign_tree(root: &str) -> usize {
    let root = Path::new(root);
    let base = if root.join(PACKAGE_DIR).is_dir() {
        root.join(PACKAGE_DIR)
    } else {
        root.to_path_buf()
    };
    if !base.is_dir() {
        return 0;
    }
    let Some(signer) = resolve_signer() else {
        log::warn!("sign: {SIGNER} not found on PATH, skipping {}", base.display());
        return 0;
    };
    let mut signed = 0;
    let mut budget = MAX_FILES;
    walk(&base, &signer, 0, &mut budget, &mut signed);
    signed
}

/// Resolves the signer from PATH, without executing it: a probe run would block
/// the command's exit report if the tool ever waited on input. Returns the first
/// PATH entry that holds an existing file of that name.
fn resolve_signer() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(SIGNER))
        .find(|candidate| candidate.is_file())
}

/// Descends `dir`, signing every unsigned ELF file found. Symlinks are never
/// followed (they would otherwise lead out of the tree or loop), and the budget
/// bounds the total work.
fn walk(
    dir: &Path,
    signer: &Path,
    depth: usize,
    budget: &mut usize,
    signed: &mut usize,
) {
    if depth > MAX_DEPTH || *budget == 0 {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            return;
        }
        let kind = match entry.file_type() {
            Ok(kind) => kind,
            Err(_) => continue,
        };
        if kind.is_symlink() {
            continue;
        }
        let path = entry.path();
        if kind.is_dir() {
            let skip = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| SKIP_DIRS.contains(&name));
            if !skip {
                walk(&path, signer, depth + 1, budget, signed);
            }
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        *budget -= 1;
        if sign_if_needed(&path, signer) {
            *signed += 1;
        }
    }
}

/// Signs `path` when it is an ELF that has not been signed yet.
fn sign_if_needed(path: &Path, signer: &Path) -> bool {
    match signature_state(path) {
        // Not an ELF, already signed, or unreadable/odd: leave it untouched.
        SignatureState::Unsigned => sign_file(path, signer),
        SignatureState::Signed | SignatureState::NotElf | SignatureState::Unknown => false,
    }
}

/// What inspection says about a file's code signature.
enum SignatureState {
    /// An ELF without a `.codesign` section -- the case worth signing.
    Unsigned,
    /// An ELF that already carries `.codesign`.
    Signed,
    /// Not an ELF at all (script, text, directory entry, ...).
    NotElf,
    /// An ELF-shaped file whose header could not be walked.
    Unknown,
}

/// Classifies a file by reading its header and section names.
fn signature_state(path: &Path) -> SignatureState {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return SignatureState::Unknown,
    };
    let mut ident = [0u8; 16];
    if file.read_exact(&mut ident).is_err() {
        return SignatureState::NotElf;
    }
    if ident[..4] != ELF_MAGIC {
        return SignatureState::NotElf;
    }
    // Only the little-endian classes this platform produces are walkable; any
    // other is left alone rather than mis-parsed.
    if ident[5] != ELFDATA2LSB || !matches!(ident[4], ELFCLASS32 | ELFCLASS64) {
        return SignatureState::Unknown;
    }
    match section_names(&mut file, ident[4]) {
        Some(names) => {
            if names.iter().any(|name| name == CODESIGN_SECTION) {
                SignatureState::Signed
            } else {
                SignatureState::Unsigned
            }
        }
        None => SignatureState::Unknown,
    }
}

/// Reads every section name out of an ELF, or `None` when the header does not
/// hold together.
fn section_names(file: &mut fs::File, class: u8) -> Option<Vec<String>> {
    // Field offsets differ between classes: the section-table pointer is a
    // 64-bit field in ELF64 and a 32-bit one in ELF32, shifting everything after
    // it. Offsets below are the System V gABI ones. Tuple is (where to find it,
    // minimum legal entry size): section-table offset, then -- relative to one
    // section header -- the section's file offset and size.
    let (shoff_at, soff_at, ssize_at, shentsize_at, shnum_at, shstrndx_at, min_entsize) =
        match class {
            ELFCLASS64 => (0x28u64, 0x18u64, 0x20u64, 0x3Au64, 0x3Cu64, 0x3Eu64, 64u64),
            _ => (0x20u64, 0x10u64, 0x14u64, 0x2Eu64, 0x30u64, 0x32u64, 40u64),
        };

    let shoff = read_field(file, shoff_at, class)?;
    let entsize = u64::from(read_u16_at(file, shentsize_at)?);
    let count = u64::from(read_u16_at(file, shnum_at)?);
    let strndx = u64::from(read_u16_at(file, shstrndx_at)?);
    if entsize < min_entsize || count == 0 || count > MAX_SECTIONS || strndx >= count {
        return None;
    }

    // Read the section-header string table in one go; every name is an offset
    // into it.
    let str_hdr = shoff.checked_add(strndx.checked_mul(entsize)?)?;
    let str_off = read_field(file, str_hdr.checked_add(soff_at)?, class)?;
    let str_size = read_field(file, str_hdr.checked_add(ssize_at)?, class)?;
    if str_size == 0 || str_size > MAX_STRTAB {
        return None;
    }
    let mut strtab = vec![0u8; str_size as usize];
    read_at(file, str_off, &mut strtab)?;

    let mut names = Vec::with_capacity(count as usize);
    for index in 0..count {
        let header = shoff.checked_add(index.checked_mul(entsize)?)?;
        let name_at = read_u32_at(file, header)? as usize;
        names.push(cstr(&strtab, name_at).unwrap_or_default());
    }
    Some(names)
}

/// Runs `signer` on one file: writes a signed copy beside it, moves that over
/// the original, and restores the execute bit the copy lacks.
fn sign_file(path: &Path, signer: &Path) -> bool {
    let Some(text) = path.to_str() else {
        return false;
    };
    let out = format!("{text}{SIGNED_SUFFIX}");
    let outcome = Command::new(signer)
        .arg("sign")
        .arg("-inFile")
        .arg(path)
        .arg("-outFile")
        .arg(&out)
        .arg("-selfSign")
        .arg("1")
        .stdin(Stdio::null())
        .output();
    let signed = matches!(outcome, Ok(ref output) if output.status.success());
    if !signed {
        let _ = fs::remove_file(&out);
        log::warn!("sign: signer failed for {}", path.display());
        return false;
    }
    if fs::rename(&out, path).is_err() {
        let _ = fs::remove_file(&out);
        return false;
    }
    use std::os::unix::fs::PermissionsExt as _;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(SIGNED_MODE));
    true
}

/// Reads `buf.len()` bytes at `offset`, or `None` when the file is shorter.
fn read_at(file: &mut fs::File, offset: u64, buf: &mut [u8]) -> Option<()> {
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(buf).ok()?;
    Some(())
}

/// Reads the 16-bit field at `offset`.
fn read_u16_at(file: &mut fs::File, offset: u64) -> Option<u16> {
    let mut buf = [0u8; 2];
    read_at(file, offset, &mut buf)?;
    Some(u16::from_le_bytes(buf))
}

/// Reads the 32-bit field at `offset`.
fn read_u32_at(file: &mut fs::File, offset: u64) -> Option<u32> {
    let mut buf = [0u8; 4];
    read_at(file, offset, &mut buf)?;
    Some(u32::from_le_bytes(buf))
}

/// Reads an ELF-width field at `offset`: 4 bytes in ELF32, 8 in ELF64.
fn read_field(file: &mut fs::File, offset: u64, class: u8) -> Option<u64> {
    if class == ELFCLASS32 {
        return read_u32_at(file, offset).map(u64::from);
    }
    let mut buf = [0u8; 8];
    read_at(file, offset, &mut buf)?;
    Some(u64::from_le_bytes(buf))
}

/// The NUL-terminated string starting at `at` in `blob`, when it is in range.
fn cstr(blob: &[u8], at: usize) -> Option<String> {
    let tail = blob.get(at..)?;
    let end = tail.iter().position(|byte| *byte == 0)?;
    std::str::from_utf8(&tail[..end]).ok().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::{cstr, ELF_MAGIC};

    #[test]
    fn reads_nul_terminated_names() {
        assert_eq!(cstr(b".text\0.data\0", 0).as_deref(), Some(".text"));
        assert_eq!(cstr(b".text\0.data\0", 6).as_deref(), Some(".data"));
    }

    #[test]
    fn rejects_out_of_range_and_unterminated() {
        assert_eq!(cstr(b".text\0", 99), None);
        assert_eq!(cstr(b".text", 0), None);
    }

    #[test]
    fn recognises_elf_magic() {
        assert_eq!(&ELF_MAGIC, b"\x7fELF");
    }
}
