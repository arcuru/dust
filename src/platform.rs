#[allow(unused_imports)]
use std::fs;

use std::path::Path;

#[cfg(all(target_family = "unix", not(target_os = "linux")))]
fn get_block_size() -> u64 {
    // All os specific implementations of MetadataExt seem to define a block as 512 bytes
    // https://doc.rust-lang.org/std/os/linux/fs/trait.MetadataExt.html#tymethod.st_blocks
    512
}

type InodeAndDevice = (u64, u64);
type FileTime = (i64, i64, i64);

/// The parsed stat fields the walker consumes.
pub type MetadataTuple = (u64, Option<InodeAndDevice>, FileTime);

#[cfg(all(target_family = "unix", not(target_os = "linux")))]
pub fn get_metadata<P: AsRef<Path>>(
    path: P,
    use_apparent_size: bool,
    follow_links: bool,
) -> Option<MetadataTuple> {
    let metadata = if follow_links {
        path.as_ref().metadata()
    } else {
        path.as_ref().symlink_metadata()
    };
    match metadata {
        Ok(md) => tuple_from_metadata(&md, use_apparent_size),
        Err(_e) => None,
    }
}

#[cfg(all(target_family = "unix", not(target_os = "linux")))]
pub fn get_metadata_and_type<P: AsRef<Path>>(
    path: P,
    use_apparent_size: bool,
    follow_links: bool,
) -> Option<(MetadataTuple, bool, bool)> {
    let metadata = if follow_links {
        path.as_ref().metadata()
    } else {
        path.as_ref().symlink_metadata()
    };
    metadata.ok().and_then(|md| {
        let is_dir = md.is_dir();
        let is_file = md.is_file();
        tuple_from_metadata(&md, use_apparent_size).map(|t| (t, is_dir, is_file))
    })
}

// On Linux, go through `statx` directly with `AT_STATX_DONT_SYNC`. The std's
// `symlink_metadata`/`metadata` path already uses `statx` under the hood, but
// with `AT_STATX_SYNC_AS_STAT` (the implicit default), which asks the
// filesystem to force any pending metadata sync before returning. For a
// read-only walker we don't need coherence with concurrent writers — we're
// happy with whatever the page cache / ARC has — and for network filesystems
// the hint can avoid a round trip. See statx(2).
//
// We extract fields directly from `struct statx` rather than synthesizing a
// `std::fs::Metadata`, because `Metadata` is opaque and can't be constructed.
// This keeps the Linux hot-path free of the detour through std.
#[cfg(target_os = "linux")]
fn statx_nosync<P: AsRef<Path>>(
    path: P,
    use_apparent_size: bool,
    follow_links: bool,
) -> Option<(MetadataTuple, u32)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path_c = CString::new(path.as_ref().as_os_str().as_bytes()).ok()?;
    let mut flags = libc::AT_STATX_DONT_SYNC | libc::AT_NO_AUTOMOUNT;
    if !follow_links {
        flags |= libc::AT_SYMLINK_NOFOLLOW;
    }
    let mask = libc::STATX_BASIC_STATS;
    let mut stx: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: `path_c` is a valid NUL-terminated C string for the duration of
    // this call; `stx` is a local of the correct type and size; flags/mask are
    // kernel-documented bit patterns.
    let ret = unsafe { libc::statx(libc::AT_FDCWD, path_c.as_ptr(), flags, mask, &mut stx) };
    if ret != 0 {
        return None;
    }

    let file_size = stx.stx_size;
    let size = if use_apparent_size {
        file_size
    } else {
        let blksize = stx.stx_blksize as u64;
        // `stx_blocks` is always in 512-byte units, regardless of blksize.
        let reported_size = stx.stx_blocks * 512;
        let target_size = file_size.div_ceil(blksize) * blksize;
        let pre_allocation_buffer = blksize * 65536;
        let max_size = target_size + pre_allocation_buffer;
        if reported_size > max_size {
            target_size
        } else {
            reported_size
        }
    };

    // Reconstruct dev_t from (major, minor) the same way glibc's makedev
    // does — matches `MetadataExt::dev()` so (ino, dev) pairs from statx
    // are comparable with those from std::fs::symlink_metadata anywhere
    // we still fall through to it.
    let major = stx.stx_dev_major as u64;
    let minor = stx.stx_dev_minor as u64;
    let dev = ((major & 0xffff_f000) << 32)
        | ((major & 0x0000_0fff) << 8)
        | ((minor & 0xffff_ff00) << 12)
        | (minor & 0x0000_00ff);

    let tuple: MetadataTuple = (
        size,
        Some((stx.stx_ino, dev)),
        (stx.stx_mtime.tv_sec, stx.stx_atime.tv_sec, stx.stx_ctime.tv_sec),
    );
    Some((tuple, stx.stx_mode as u32))
}

#[cfg(target_os = "linux")]
pub fn get_metadata<P: AsRef<Path>>(
    path: P,
    use_apparent_size: bool,
    follow_links: bool,
) -> Option<MetadataTuple> {
    statx_nosync(path, use_apparent_size, follow_links).map(|(t, _)| t)
}

#[cfg(target_os = "linux")]
pub fn get_metadata_and_type<P: AsRef<Path>>(
    path: P,
    use_apparent_size: bool,
    follow_links: bool,
) -> Option<(MetadataTuple, bool, bool)> {
    statx_nosync(path, use_apparent_size, follow_links).map(|(tuple, mode)| {
        let ifmt = mode & (libc::S_IFMT as u32);
        let is_dir = ifmt == libc::S_IFDIR as u32;
        let is_file = ifmt == libc::S_IFREG as u32;
        (tuple, is_dir, is_file)
    })
}

/// Extract the data tuple from an already-fetched `Metadata`, no syscall.
/// Used on non-Linux Unix (macOS, BSDs) where we still go through std's
/// `fs::Metadata`. On Linux we skip this entirely and read fields from
/// `statx` directly.
#[cfg(all(target_family = "unix", not(target_os = "linux")))]
fn tuple_from_metadata(
    md: &std::fs::Metadata,
    use_apparent_size: bool,
) -> Option<MetadataTuple> {
    use std::os::unix::fs::MetadataExt;
    let file_size = md.len();
    if use_apparent_size {
        Some((
            file_size,
            Some((md.ino(), md.dev())),
            (md.mtime(), md.atime(), md.ctime()),
        ))
    } else {
        // On NTFS mounts, the reported block count can be unexpectedly large.
        // To avoid overestimating disk usage, cap the allocated size to what the
        // file should occupy based on the file system I/O block size (blksize).
        // Related: https://github.com/bootandy/dust/issues/295
        let blksize = md.blksize();
        let target_size = file_size.div_ceil(blksize) * blksize;
        let reported_size = md.blocks() * get_block_size();

        // File systems can pre-allocate more space for a file than what would be necessary
        let pre_allocation_buffer = blksize * 65536;
        let max_size = target_size + pre_allocation_buffer;
        let allocated_size = if reported_size > max_size {
            target_size
        } else {
            reported_size
        };
        Some((
            allocated_size,
            Some((md.ino(), md.dev())),
            (md.mtime(), md.atime(), md.ctime()),
        ))
    }
}

#[cfg(target_family = "windows")]
pub fn get_metadata<P: AsRef<Path>>(
    path: P,
    use_apparent_size: bool,
    follow_links: bool,
) -> Option<MetadataTuple> {
    // On windows opening the file to get size, file ID and volume can be very
    // expensive because 1) it causes a few system calls, and more importantly 2) it can cause
    // windows defender to scan the file.
    // Therefore we try to avoid doing that for common cases, mainly those of
    // plain files:

    // The idea is to make do with the file size that we get from the OS for
    // free as part of iterating a folder. Therefore we want to make sure that
    // it makes sense to use that free size information:

    // Volume boundaries:
    // The user can ask us not to cross volume boundaries. If the DirEntry is a
    // plain file and not a reparse point or other non-trivial stuff, we assume
    // that the file is located on the same volume as the directory that
    // contains it.

    // File ID:
    // This optimization does deprive us of access to a file ID. As a
    // workaround, we just make one up that hopefully does not collide with real
    // file IDs.
    // Hard links: Unresolved. We don't get inode/file index, so hard links
    // count once for each link. Hopefully they are not too commonly in use on
    // windows.

    // Size:
    // We assume (naively?) that for the common cases the free size info is the
    // same as one would get by doing the expensive thing. Sparse, encrypted and
    // compressed files are not included in the common cases, as one can image
    // there being more than view on their size.

    // Savings in orders of magnitude in terms of time, io and cpu have been
    // observed on hdd, windows 10, some 100Ks files taking up some hundreds of
    // GBs:
    // Consistently opening the file: 30 minutes.
    // With this optimization:         8 sec.

    use std::io;
    use winapi_util::Handle;
    fn handle_from_path_limited(path: &Path) -> io::Result<Handle> {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_READ_ATTRIBUTES: u32 = 0x0080;

        // So, it seems that it does does have to be that expensive to open
        // files to get their info: Avoiding opening the file with the full
        // GENERIC_READ is key:

        // https://docs.microsoft.com/en-us/windows/win32/secauthz/generic-access-rights:
        // "For example, a Windows file object maps the GENERIC_READ bit to the
        // READ_CONTROL and SYNCHRONIZE standard access rights and to the
        // FILE_READ_DATA, FILE_READ_EA, and FILE_READ_ATTRIBUTES
        // object-specific access rights"

        // The flag FILE_READ_DATA seems to be the expensive one, so we'll avoid
        // that, and a most of the other ones. Simply because it seems that we
        // don't need them.

        let file = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .open(path)?;
        Ok(Handle::from_file(file))
    }

    fn get_metadata_expensive(path: &Path, use_apparent_size: bool) -> Option<MetadataTuple> {
        use winapi_util::file::information;

        let h = handle_from_path_limited(path).ok()?;
        let info = information(&h).ok()?;

        if use_apparent_size {
            use filesize::PathExt;
            Some((
                path.size_on_disk().ok()?,
                Some((info.file_index(), info.volume_serial_number())),
                (
                    info.last_write_time().unwrap() as i64,
                    info.last_access_time().unwrap() as i64,
                    info.creation_time().unwrap() as i64,
                ),
            ))
        } else {
            Some((
                info.file_size(),
                Some((info.file_index(), info.volume_serial_number())),
                (
                    info.last_write_time().unwrap() as i64,
                    info.last_access_time().unwrap() as i64,
                    info.creation_time().unwrap() as i64,
                ),
            ))
        }
    }

    let path = path.as_ref();
    let metadata = if follow_links {
        path.metadata()
    } else {
        path.symlink_metadata()
    };
    match metadata {
        Ok(ref md) => tuple_from_metadata(md, use_apparent_size)
            .or_else(|| get_metadata_expensive(path, use_apparent_size)),
        _ => get_metadata_expensive(path, use_apparent_size),
    }
}

#[cfg(target_family = "windows")]
pub fn get_metadata_and_type<P: AsRef<Path>>(
    path: P,
    use_apparent_size: bool,
    follow_links: bool,
) -> Option<((u64, Option<InodeAndDevice>, FileTime), bool, bool)> {
    let path = path.as_ref();
    let metadata = if follow_links {
        path.metadata()
    } else {
        path.symlink_metadata()
    };
    match metadata {
        Ok(md) => {
            let is_dir = md.is_dir();
            let is_file = md.is_file();
            let tuple = tuple_from_metadata(&md, use_apparent_size)
                .or_else(|| get_metadata(path, use_apparent_size, follow_links))?;
            Some((tuple, is_dir, is_file))
        }
        Err(_) => None,
    }
}

/// Extract the data tuple from an already-fetched `Metadata` on Windows.
/// Returns `None` when the file needs the expensive (handle-open) path —
/// the caller must fall back. Directories and normal files always return
/// `Some` (that's the cheap branch the current code already takes).
#[cfg(target_family = "windows")]
pub fn tuple_from_metadata(
    md: &std::fs::Metadata,
    use_apparent_size: bool,
) -> Option<MetadataTuple> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
    const FILE_ATTRIBUTE_READONLY: u32 = 0x01;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x02;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x04;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x00000200;
    const FILE_ATTRIBUTE_PINNED: u32 = 0x00080000;
    const FILE_ATTRIBUTE_UNPINNED: u32 = 0x00100000;
    const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x00040000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x00400000;
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x00001000;
    // normally FILE_ATTRIBUTE_SPARSE_FILE would be enough, however Windows sometimes likes to mask it out. see: https://stackoverflow.com/q/54560454
    const IS_PROBABLY_ONEDRIVE: u32 = FILE_ATTRIBUTE_SPARSE_FILE
        | FILE_ATTRIBUTE_PINNED
        | FILE_ATTRIBUTE_UNPINNED
        | FILE_ATTRIBUTE_RECALL_ON_OPEN
        | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
        | FILE_ATTRIBUTE_OFFLINE;
    let attr_filtered = md.file_attributes()
        & !(FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_SYSTEM);
    if ((attr_filtered & FILE_ATTRIBUTE_ARCHIVE) != 0
        || (attr_filtered & FILE_ATTRIBUTE_DIRECTORY) != 0
        || md.file_attributes() == FILE_ATTRIBUTE_NORMAL)
        && !((attr_filtered & IS_PROBABLY_ONEDRIVE != 0) && use_apparent_size)
    {
        Some((
            md.len(),
            None,
            (
                md.last_write_time() as i64,
                md.last_access_time() as i64,
                md.creation_time() as i64,
            ),
        ))
    } else {
        None
    }
}
