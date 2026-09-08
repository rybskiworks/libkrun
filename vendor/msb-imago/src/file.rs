//! Use a plain file or host block device as storage.

#[cfg(unix)]
use crate::io_buffers::IoBuffer;
use crate::io_buffers::{IoVector, IoVectorMut};
#[cfg(unix)]
use crate::misc_helpers::while_eintr;
use crate::misc_helpers::ResultErrorContext;
use crate::storage::drivers::CommonStorageHelper;
use crate::storage::ext::write_full_zeroes;
use crate::storage::PreallocateMode;
use crate::{Storage, StorageCreateOptions, StorageOpenOptions};
use cfg_if::cfg_if;
use std::fmt::{self, Display, Formatter};
use std::io::{self, Write};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
#[cfg(all(unix, not(target_os = "macos")))]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::fs::{FileExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::RwLock;
use std::{cmp, fs};
#[cfg(unix)]
use tracing::{debug, warn};
#[cfg(windows)]
use windows_sys::Win32::System::Ioctl::{FILE_ZERO_DATA_INFORMATION, FSCTL_SET_ZERO_DATA};
#[cfg(windows)]
use windows_sys::Win32::System::IO::DeviceIoControl;

/// Linux UAPI value for `RWF_DONTCACHE`.
///
/// Keeping the stable UAPI bit here avoids raising Imago's minimum `libc` crate version solely
/// for a newly exposed constant. Both supported Linux libc targets already expose `pwritev2()`.
#[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
const RWF_DONTCACHE_FLAG: libc::c_int = 0x0000_0080;

/// Use a plain file or host block device as a storage object.
#[derive(Debug)]
pub struct File {
    /// The file.
    file: RwLock<fs::File>,

    /// For debug purposes, and to resolve relative filenames.
    filename: Option<PathBuf>,

    /// Minimal I/O alignment for requests.
    req_align: usize,

    /// Minimal memory buffer alignment.
    mem_align: usize,

    /// Minimum required alignment for zero writes.
    zero_align: usize,

    /// Minimum required alignment for effective discards.
    discard_align: usize,

    /// Cached file length.
    ///
    /// Third parties changing the length concurrently is pretty certain to break things anyway.
    size: AtomicU64,

    /// Storage helper.
    common_storage_helper: CommonStorageHelper,

    /// macOS-only: Use fsync() instead of F_FULLFSYNC on `sync()` method.
    #[cfg(target_os = "macos")]
    relaxed_sync: bool,

    /// GNU/musl Linux-only: Whether to try `RWF_DONTCACHE` for buffered writes.
    ///
    /// The flag is cleared permanently after the kernel or filesystem reports the hint as
    /// unsupported, avoiding repeated failed syscalls while preserving ordinary buffered writes.
    #[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
    write_dontcache: AtomicBool,

    /// Set once we know that discard is unsupported and we can skip trying.
    discard_unsupported: AtomicBool,
}

impl TryFrom<fs::File> for File {
    type Error = io::Error;

    /// Use the given existing `std::fs::File`.
    ///
    /// Convert the given existing `std::fs::File` object into an imago storage object.
    ///
    /// When using this, the resulting object will not know its own filename.  That makes it
    /// impossible to auto-resolve relative paths to it, e.g. qcow2 backing file names.
    fn try_from(file: fs::File) -> io::Result<Self> {
        Self::new(
            file,
            None,
            false,
            #[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
            false,
            #[cfg(target_os = "macos")]
            false,
        )
    }
}

impl Storage for File {
    async fn open(opts: StorageOpenOptions) -> io::Result<Self> {
        Self::do_open_sync(opts, fs::OpenOptions::new())
    }

    #[cfg(feature = "sync-wrappers")]
    fn open_sync(opts: StorageOpenOptions) -> io::Result<Self> {
        Self::do_open_sync(opts, fs::OpenOptions::new())
    }

    async fn create_open(opts: StorageCreateOptions) -> io::Result<Self> {
        // Always allow writing for new files
        let opts = opts.modify_open_opts(|o| o.write(true));
        let size = opts.size;
        let prealloc_mode = opts.prealloc_mode;

        let mut file_opts = fs::OpenOptions::new();
        if opts.overwrite {
            file_opts.create(true).truncate(true);
        } else {
            file_opts.create_new(true);
        };

        let file = Self::do_open_sync(opts.get_open_options(), file_opts)?;
        if size > 0 {
            file.resize(size, prealloc_mode)
                .await
                .err_context(|| "Resizing file")?;
        }

        Ok(file)
    }

    fn mem_align(&self) -> usize {
        self.mem_align
    }

    fn req_align(&self) -> usize {
        self.req_align
    }

    fn zero_align(&self) -> usize {
        self.zero_align
    }

    fn discard_align(&self) -> usize {
        self.discard_align
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.size.load(Ordering::Relaxed))
    }

    fn resolve_relative_path<P: AsRef<Path>>(&self, relative: P) -> io::Result<PathBuf> {
        let relative = relative.as_ref();

        if relative.is_absolute() {
            return Ok(relative.to_path_buf());
        }

        let filename = self
            .filename
            .as_ref()
            .ok_or_else(|| io::Error::other("No filename set for base image"))?;

        let dirname = filename
            .parent()
            .ok_or_else(|| io::Error::other("Invalid base image filename set"))?;

        Ok(dirname.join(relative))
    }

    fn get_filename(&self) -> Option<PathBuf> {
        self.filename.as_ref().cloned()
    }

    #[cfg(unix)]
    async unsafe fn pure_readv(
        &self,
        mut bufv: IoVectorMut<'_>,
        mut offset: u64,
    ) -> io::Result<()> {
        while !bufv.is_empty() {
            let iovec = unsafe { bufv.as_iovec() };
            let preadv_offset = offset
                .try_into()
                .map_err(|_| io::Error::other("Read offset overflow"))?;

            let len = while_eintr(|| unsafe {
                libc::preadv(
                    self.file.read().unwrap().as_raw_fd(),
                    iovec.as_ptr(),
                    iovec.len() as libc::c_int,
                    preadv_offset,
                )
            })? as u64;

            if len == 0 {
                // End of file
                bufv.fill(0);
                break;
            }

            bufv = bufv.split_tail_at(len);
            offset = offset
                .checked_add(len)
                .ok_or_else(|| io::Error::other("Read offset overflow"))?;
        }

        Ok(())
    }

    #[cfg(windows)]
    async unsafe fn pure_readv(&self, bufv: IoVectorMut<'_>, mut offset: u64) -> io::Result<()> {
        for mut buffer in bufv.into_inner() {
            let mut buffer: &mut [u8] = &mut buffer;
            while !buffer.is_empty() {
                let len = if offset >= self.size.load(Ordering::Relaxed) {
                    buffer.fill(0);
                    buffer.len()
                } else {
                    self.file.write().unwrap().seek_read(buffer, offset)?
                };
                offset = offset
                    .checked_add(len as u64)
                    .ok_or_else(|| io::Error::other("Read offset overflow"))?;
                buffer = buffer.split_at_mut(len).1;
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    async unsafe fn pure_writev(&self, mut bufv: IoVector<'_>, mut offset: u64) -> io::Result<()> {
        while !bufv.is_empty() {
            let iovec = unsafe { bufv.as_iovec() };
            let pwritev_offset = offset
                .try_into()
                .map_err(|_| io::Error::other("Write offset overflow"))?;

            #[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
            let len = write_with_optional_dontcache(
                &self.write_dontcache,
                || {
                    // Safe: The descriptor and iovec remain valid for the duration of the call;
                    // the offset was checked above and the flag has no pointer arguments.
                    syscall_result(unsafe {
                        libc::pwritev2(
                            self.file.read().unwrap().as_raw_fd(),
                            iovec.as_ptr(),
                            iovec.len() as libc::c_int,
                            pwritev_offset,
                            RWF_DONTCACHE_FLAG,
                        )
                    })
                },
                || {
                    // This fallback receives the exact same unconsumed iovec and offset.  It is
                    // used only when the kernel explicitly rejects `RWF_DONTCACHE`.
                    syscall_result(unsafe {
                        libc::pwritev(
                            self.file.read().unwrap().as_raw_fd(),
                            iovec.as_ptr(),
                            iovec.len() as libc::c_int,
                            pwritev_offset,
                        )
                    })
                },
            )?;

            #[cfg(all(
                unix,
                not(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))
            ))]
            let len = while_eintr(|| unsafe {
                libc::pwritev(
                    self.file.read().unwrap().as_raw_fd(),
                    iovec.as_ptr(),
                    iovec.len() as libc::c_int,
                    pwritev_offset,
                )
            })?;

            let len = require_write_progress(len)?;

            bufv = bufv.split_tail_at(len);
            offset = offset
                .checked_add(len)
                .ok_or_else(|| io::Error::other("Write offset overflow"))?;
            self.size.fetch_max(offset, Ordering::Relaxed);
        }

        Ok(())
    }

    #[cfg(windows)]
    async unsafe fn pure_writev(&self, bufv: IoVector<'_>, mut offset: u64) -> io::Result<()> {
        for buffer in bufv.into_inner() {
            let mut buffer: &[u8] = &buffer;
            while !buffer.is_empty() {
                let len = self.file.write().unwrap().seek_write(buffer, offset)?;
                offset = offset
                    .checked_add(len as u64)
                    .ok_or_else(|| io::Error::other("Write offset overflow"))?;
                self.size.fetch_max(offset, Ordering::Relaxed);
                buffer = buffer.split_at(len).1;
            }
        }
        Ok(())
    }

    async unsafe fn pure_write_zeroes(&self, offset: u64, length: u64) -> io::Result<()> {
        self.discard_to_zero(offset, length).await
    }

    #[cfg(target_os = "linux")]
    async unsafe fn pure_write_allocated_zeroes(&self, offset: u64, length: u64) -> io::Result<()> {
        let offset: libc::off_t = offset
            .try_into()
            .map_err(|e| io::Error::other(format!("Discard/write-zeroes offset error: {e}")))?;
        let length: libc::off_t = length
            .try_into()
            .map_err(|e| io::Error::other(format!("Discard/write-zeroes length error: {e}")))?;

        let file = self.file.read().unwrap();
        // Safe: File descriptor is valid, and the rest are simple integer parameters.
        while_eintr(|| unsafe {
            libc::fallocate(file.as_raw_fd(), libc::FALLOC_FL_ZERO_RANGE, offset, length)
        })
        .map_err(Self::map_os_err)?;

        Ok(())
    }

    async unsafe fn pure_discard(&self, offset: u64, length: u64) -> io::Result<()> {
        if let Err(err) = self.discard_to_zero(offset, length).await {
            // Ignore `Unsupported` errors: As per the `pure_discard` documentation, a no-op
            // implementation is acceptable.  In addition, the default implementation returns
            // `Ok(())`, and it makes no sense to be harsher than that here.
            if err.kind() == io::ErrorKind::Unsupported {
                Ok(())
            } else {
                Err(err)
            }
        } else {
            Ok(())
        }
    }

    async fn flush(&self) -> io::Result<()> {
        self.file.write().unwrap().flush()
    }

    async fn sync(&self) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        if self.relaxed_sync {
            // Safe: File descriptor is valid and there aren't any other arguments.
            while_eintr(|| unsafe { libc::fsync(self.file.write().unwrap().as_raw_fd()) })?;
            return Ok(());
        }
        self.file.write().unwrap().sync_all()
    }

    async unsafe fn invalidate_cache(&self) -> io::Result<()> {
        // TODO: Figure out what to do.  Generally, `std::fs::File` does not have internal buffers,
        // so we don’t need to invalidate anything; we could close and reopen, but that would still
        // flush, and is difficult to do in a platform-independent way (/proc/self/fd would allow
        // this on Linux).  Using e.g. the filename is not safe.
        // Right now, it’s best not to do anything.
        Ok(())
    }

    fn get_storage_helper(&self) -> &CommonStorageHelper {
        &self.common_storage_helper
    }

    async fn resize(&self, new_size: u64, prealloc_mode: PreallocateMode) -> io::Result<()> {
        let file = self.file.write().unwrap();
        let current_size = self.size.load(Ordering::Relaxed);

        match new_size.cmp(&current_size) {
            std::cmp::Ordering::Equal => return Ok(()),
            std::cmp::Ordering::Less => {
                file.set_len(new_size)?;
                self.size.fetch_min(new_size, Ordering::Relaxed);
                return Ok(());
            }
            std::cmp::Ordering::Greater => (), // handled below
        }

        match prealloc_mode {
            PreallocateMode::None | PreallocateMode::Zero => file.set_len(new_size)?,
            PreallocateMode::Allocate => {
                #[cfg(not(unix))]
                return Err(io::ErrorKind::Unsupported.into());

                #[cfg(all(unix, not(target_os = "macos")))]
                {
                    let ofs = current_size.try_into().map_err(io::Error::other)?;
                    let len = (new_size - current_size)
                        .try_into()
                        .map_err(io::Error::other)?;
                    while_eintr(|| unsafe { libc::fallocate(file.as_raw_fd(), 0, ofs, len) })
                        .map_err(Self::map_os_err)?;
                }

                #[cfg(target_os = "macos")]
                {
                    // Best-effort.  PEOFPOSMODE allocates from the “physical” EOF, wherever that
                    // may be, but the only alternative would be VOLPOSMODE, which nobody knows the
                    // meaning of.  Also doesn’t change the file length, we need to truncate
                    // afterwards still.
                    let mut params = libc::fstore_t {
                        fst_flags: libc::F_ALLOCATEALL,
                        fst_posmode: libc::F_PEOFPOSMODE,
                        fst_offset: 0,
                        fst_length: (new_size - current_size)
                            .try_into()
                            .map_err(io::Error::other)?,
                        fst_bytesalloc: 0, // output
                    };
                    while_eintr(|| unsafe {
                        libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &mut params)
                    })
                    .map_err(Self::map_os_err)?;

                    file.set_len(new_size)?;
                }
            }
            PreallocateMode::WriteData => {
                // FIXME: Keeping the lock would be nice, but resizing concurrently with I/O is
                // pretty risky anyway.
                drop(file);
                write_full_zeroes(self, current_size, new_size - current_size).await?;
            }
        }

        self.size.fetch_max(new_size, Ordering::Relaxed);
        Ok(())
    }
}

impl File {
    /// Central internal function to create a `File` object.
    ///
    /// `direct_io` should be `true` if direct I/O was requested, and can be `false` if that status
    /// is unknown.
    fn new(
        mut file: fs::File,
        filename: Option<PathBuf>,
        direct_io: bool,
        #[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
        write_dontcache: bool,
        #[cfg(target_os = "macos")] relaxed_sync: bool,
    ) -> io::Result<Self> {
        let size = get_file_size(&file).err_context(|| "Failed to determine file size")?;

        #[cfg(all(unix, not(target_os = "macos")))]
        let direct_io = direct_io || {
            // Safe: No argument, returns result.
            let res = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
            res > 0 && (res & libc::O_DIRECT) != 0
        };

        let (min_req_align, min_mem_align) = if direct_io {
            #[cfg(unix)]
            {
                (
                    Self::get_min_dio_req_align(&file),
                    Self::get_min_dio_mem_align(&file),
                )
            }

            #[cfg(not(unix))]
            {
                (1, 1)
            } // probe it then
        } else {
            (1, 1)
        };

        let (req_align, mem_align, zero_align, discard_align) =
            Self::probe_alignments(&mut file, min_req_align, min_mem_align);
        assert!(req_align.is_power_of_two());
        assert!(mem_align.is_power_of_two());

        Ok(File {
            file: RwLock::new(file),
            filename,
            req_align,
            mem_align,
            zero_align,
            discard_align,
            size: size.into(),
            common_storage_helper: Default::default(),
            #[cfg(target_os = "macos")]
            relaxed_sync,
            #[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
            write_dontcache: AtomicBool::new(write_dontcache),
            discard_unsupported: AtomicBool::new(false),
        })
    }

    /// Probe minimal request, memory, zero and discard alignments.
    ///
    /// Start at `min_req_align` and `min_mem_align`.
    #[cfg(unix)]
    fn probe_alignments(
        file: &mut fs::File,
        min_req_align: usize,
        min_mem_align: usize,
    ) -> (usize, usize, usize, usize) {
        let mut page_size = page_size::get();
        if !page_size.is_power_of_two() {
            let assume = page_size.checked_next_power_of_two().unwrap_or(4096);
            let assume = cmp::max(4096, assume);
            warn!("Reported page size of {page_size} is not a power of two, assuming {assume}");
            page_size = assume;
        }

        #[cfg(not(target_os = "macos"))]
        let (zero_align, discard_align) = (1, 1);
        #[cfg(target_os = "macos")]
        let (zero_align, discard_align) = {
            let mut statfs: libc::statfs = unsafe { std::mem::zeroed() };
            // Safe: FD is valid, passed pointer is valid and its type matches the call.
            match while_eintr(|| unsafe { libc::fstatfs(file.as_raw_fd(), &mut statfs) }) {
                Ok(_) => (statfs.f_bsize as usize, statfs.f_bsize as usize),
                Err(_) => (page_size, page_size),
            }
        };

        let mut writable = true;

        let max_req_align = 65536;
        let max_mem_align = cmp::max(page_size, max_req_align);

        // Minimum fallbacks in case something goes wrong.
        let safe_req_align = 4096;
        let safe_mem_align = cmp::max(page_size, safe_req_align);

        let mut test_buf = match IoBuffer::new(max_mem_align, max_mem_align) {
            Ok(buf) => buf,
            Err(err) => {
                warn!(
                    "Failed to allocate memory to probe request alignment ({err}), \
                    falling back to {safe_req_align}/{safe_mem_align}"
                );
                return (safe_req_align, safe_mem_align, zero_align, discard_align);
            }
        };

        let mut req_align: usize = min_req_align;
        let result = loop {
            assert!(req_align <= max_mem_align);
            match Self::probe_access(
                file,
                test_buf.as_mut_range(0..req_align).into_slice(),
                req_align.try_into().unwrap(),
                &mut writable,
            ) {
                Ok(true) => break Ok(req_align),
                Ok(false) => {
                    if req_align >= max_req_align {
                        break Err(io::Error::other(format!(
                            "Maximum I/O alignment ({max_req_align}) exceeded"
                        )));
                    }
                    // No reason to probe anything between 1 and 512
                    if req_align == min_req_align {
                        req_align = cmp::max(min_req_align << 1, 512);
                    } else {
                        req_align <<= 1;
                    }
                }
                Err(err) => break Err(err),
            }
        };

        let req_align = match result {
            Ok(align) => {
                debug!("Probed request alignment: {align}");
                align
            }
            Err(err) => {
                // Failed to determine request alignment, use a presumably safe value
                let align = cmp::max(req_align, safe_req_align);
                warn!(
                    "Failed to probe request alignment ({err}; {}), falling back to {align} bytes",
                    err.kind(),
                );
                align
            }
        };

        let mut mem_align: usize = min_mem_align;
        let result = loop {
            assert!(mem_align <= max_mem_align);
            let range = (max_mem_align - mem_align)..max_mem_align;
            match Self::probe_access(
                file,
                test_buf.as_mut_range(range).into_slice(),
                0,
                &mut writable,
            ) {
                Ok(true) => break Ok(mem_align),
                Ok(false) => {
                    // Not aligned
                    if mem_align >= max_mem_align {
                        break Err(io::Error::other(format!(
                            "Maximum memory alignment ({max_mem_align}) exceeded"
                        )));
                    }
                    // No reason to probe anything between 1 and the page size (or 4096 at least)
                    if mem_align == min_mem_align {
                        mem_align = cmp::max(min_mem_align << 1, cmp::min(page_size, 4096));
                    } else {
                        mem_align <<= 1;
                    }
                }
                Err(err) => break Err(err),
            }
        };

        let mem_align = match result {
            Ok(align) => {
                debug!("Probed memory alignment: {align}");
                align
            }
            Err(err) => {
                // Failed to determine memory alignment, use a presumably safe value
                let align = cmp::max(mem_align, safe_mem_align);
                warn!(
                    "Failed to probe memory alignment ({err}; {}), falling back to {align} bytes",
                    err.kind(),
                );
                align
            }
        };

        (req_align, mem_align, zero_align, discard_align)
    }

    /// Do an alignment-probing I/O access.
    ///
    /// Return `Ok(true)` if everything was OK, and `Ok(false)` if the request was reported to be
    /// misaligned.
    ///
    /// `may_write` is a boolean that controls whether this is allowed to write (the same data read
    /// before) to improve reliability.  Is automatically set to `false` if writing is found to not
    /// be possible.
    #[cfg(unix)]
    fn probe_access(
        file: &mut fs::File,
        slice: &mut [u8],
        offset: libc::off_t,
        may_write: &mut bool,
    ) -> io::Result<bool> {
        // Use `libc::pread` so we get well-defined errors.
        // Safe: Passing the slice as the buffer it is.
        let ret = while_eintr(|| unsafe {
            libc::pread(
                file.as_raw_fd(),
                slice.as_mut_ptr() as *mut libc::c_void,
                slice.len(),
                offset,
            )
        });

        if let Err(err) = ret {
            if err.raw_os_error() == Some(libc::EINVAL) {
                return Ok(false);
            } else {
                return Err(err);
            }
        }

        if !*may_write {
            return Ok(true);
        }

        // Safe: Passing the slice as the buffer it is.
        let ret = while_eintr(|| unsafe {
            libc::pwrite(
                file.as_raw_fd(),
                slice.as_ptr() as *const libc::c_void,
                slice.len(),
                offset,
            )
        });

        if let Err(err) = ret {
            if err.raw_os_error() == Some(libc::EINVAL) {
                Ok(false)
            } else if err.raw_os_error() == Some(libc::EBADF) {
                *may_write = false;
                Ok(true)
            } else {
                Err(err)
            }
        } else {
            Ok(true)
        }
    }

    /// Get system-reported minimum request alignment for direct I/O.
    #[cfg(unix)]
    fn get_min_dio_req_align(file: &fs::File) -> usize {
        #[cfg(target_os = "linux")]
        {
            let mut alignment = 0;
            let res = unsafe { ioctl::blksszget(file.as_raw_fd(), &mut alignment) };
            if res.is_ok() && alignment > 0 {
                let alignment = alignment as usize;
                if alignment.is_power_of_two() {
                    return alignment;
                }
            }
        }

        #[cfg(target_os = "macos")]
        {
            let mut alignment = 0;
            let res = unsafe { ioctl::dkiocgetblocksize(file.as_raw_fd(), &mut alignment) };
            if res.is_ok() && alignment.is_power_of_two() {
                return alignment as usize;
            }
        }

        #[cfg(target_os = "freebsd")]
        {
            let mut alignment = 0;
            let res = unsafe { ioctl::diocgsectorsize(file.as_raw_fd(), &mut alignment) };
            if res.is_ok() && alignment.is_power_of_two() {
                return alignment as usize;
            }
        }

        // Then we’ll probe.
        1
    }

    /// Get system-reported minimum memory alignment for direct I/O.
    #[cfg(unix)]
    fn get_min_dio_mem_align(_file: &fs::File) -> usize {
        // I don’t think there’s a reliable way to get this.
        1
    }

    /// Probe minimal request and memory alignments.
    ///
    /// Start at `min_req_align` and `min_mem_align`.
    #[cfg(windows)]
    fn probe_alignments(
        _file: &mut fs::File,
        min_req_align: usize,
        min_mem_align: usize,
    ) -> (usize, usize, usize, usize) {
        // TODO: Need to find out how Windows indicates unaligned I/O
        (
            cmp::max(min_req_align, 4096),
            cmp::max(min_mem_align, 4096),
            1,
            1,
        )
    }

    /// Implementation for anything that opens a file.
    fn do_open_sync(opts: StorageOpenOptions, base_fs_opts: fs::OpenOptions) -> io::Result<Self> {
        #[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
        if opts.write_dontcache && !opts.writable {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RWF_DONTCACHE requires writable storage",
            ));
        }

        #[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
        if opts.write_dontcache && opts.direct {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RWF_DONTCACHE is incompatible with direct I/O",
            ));
        }

        let Some(filename) = opts.filename else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Filename required",
            ));
        };

        let mut file_opts = base_fs_opts;
        file_opts.read(true).write(opts.writable);
        #[cfg(not(target_os = "macos"))]
        if opts.direct {
            file_opts.custom_flags(
                #[cfg(unix)]
                libc::O_DIRECT,
                #[cfg(windows)]
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_NO_BUFFERING,
            );
        }

        let filename_owned = filename.to_owned();
        let file = file_opts.open(filename)?;

        #[cfg(target_os = "macos")]
        if opts.direct {
            // Safe: We check the return value.
            while_eintr(|| unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) })
                .err_context(|| "Failed to disable host cache")?;
        }

        Self::new(
            file,
            Some(filename_owned),
            opts.direct,
            #[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
            opts.write_dontcache,
            #[cfg(target_os = "macos")]
            opts.relaxed_sync,
        )
    }

    /// For special operations, ensure the error kind is usable.
    ///
    /// When invoking OS I/O operations directly, we turn the returned raw OS error code into an
    /// `io::Error` object via `io::Error::last_os_error()`.  To differentiate between different
    /// error cases, in generic imago code, we then don’t use that raw error code, but the error
    /// kind (`io::ErrorKind`) instead, specifically it’s important to properly return an error of
    /// kind `io::ErrorKind::Unsupported` when an operation is unsupported, so fall-backs can be
    /// employed.
    ///
    /// Rust’s standard library only assigns this error kind (`Unsupported`) to `EOPNOTSUPP` (=
    /// `ENOTSUP`) and `ENOSYS`.  However, some “special” operations (`fallocate()`,
    /// `fcntl(F_PUNCHHOLE)`, `ioctl()`, ...) can return other error codes for when an operation is
    /// not supported on a specific file, e.g. `ENODEV` or `ENXIO`.
    ///
    /// Assign the appropriate error kind to such errors so the generic code can handle them.
    #[cfg(unix)]
    fn map_os_err(err: io::Error) -> io::Error {
        let Some(raw) = err.raw_os_error() else {
            return err;
        };

        let has_kind = err.kind();
        let want_kind = match raw {
            #[allow(unreachable_patterns)] // `ENOTSUP` may be equal to `EOPNOTSUPP`
            libc::ENOTSUP | libc::EOPNOTSUPP | libc::ENODEV | libc::ENXIO | libc::ENOTTY => {
                io::ErrorKind::Unsupported
            }
            _ => has_kind,
        };

        if has_kind != want_kind {
            io::Error::new(want_kind, err)
        } else {
            err
        }
    }

    /// For special operations, ensure the error kind is usable.
    ///
    /// For non-UNIX systems, this is an identity map.
    #[cfg(not(unix))]
    fn map_os_err(err: io::Error) -> io::Error {
        err
    }

    /// Attempt to discard range by truncating the file.
    ///
    /// If the given range is at the end of the file, discard it by simply truncating the file.
    /// Return `true` on success.
    ///
    /// If the range is not at the end of the file, i.e. another method of discarding is needed,
    /// return `false`.
    fn try_discard_by_truncate(&self, offset: u64, length: u64) -> io::Result<bool> {
        // Prevent modifications to the file length
        #[allow(clippy::readonly_write_lock)]
        let file = self.file.write().unwrap();

        let size = self.size.load(Ordering::Relaxed);
        if offset >= size {
            // Nothing to do
            return Ok(true);
        }

        // If `offset + length` overflows, we can just assume it ends at `size`.  (Anything past
        // `size is irrelevant anyway.)
        let end = offset.checked_add(length).unwrap_or(size);
        if end < size {
            return Ok(false);
        }

        file.set_len(offset)?;
        Ok(true)
    }

    /// Ensure the given range reads back as zeroes, or return an error.
    async fn discard_to_zero(&self, offset: u64, length: u64) -> io::Result<()> {
        if self.try_discard_by_truncate(offset, length)? {
            return Ok(());
        }

        if self.discard_unsupported.load(Ordering::Relaxed) {
            Err(io::ErrorKind::Unsupported.into())
        } else if let Err(err) = self.discard_to_zero_os_specific(offset, length).await {
            if err.kind() == io::ErrorKind::Unsupported {
                self.discard_unsupported.store(true, Ordering::Relaxed);
            }
            Err(err)
        } else {
            Ok(())
        }
    }

    /// Via OS-specific means, ensure the given range reads back as zeroes, or return an error.
    #[cfg(target_os = "linux")]
    async fn discard_to_zero_os_specific(&self, offset: u64, length: u64) -> io::Result<()> {
        let offset: libc::off_t = offset
            .try_into()
            .map_err(|e| io::Error::other(format!("Discard/write-zeroes offset error: {e}")))?;
        let length: libc::off_t = length
            .try_into()
            .map_err(|e| io::Error::other(format!("Discard/write-zeroes length error: {e}")))?;

        let file = self.file.read().unwrap();
        // Safe: File descriptor is valid, and the rest are simple integer parameters.
        while_eintr(|| unsafe {
            libc::fallocate(
                file.as_raw_fd(),
                libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                offset,
                length,
            )
        })
        .map_err(Self::map_os_err)?;

        Ok(())
    }

    /// Via OS-specific means, ensure the given range reads back as zeroes, or return an error.
    #[cfg(windows)]
    async fn discard_to_zero_os_specific(&self, offset: u64, length: u64) -> io::Result<()> {
        let offset: i64 = offset
            .try_into()
            .map_err(|e| io::Error::other(format!("Discard/write-zeroes offset error: {e}")))?;
        let length: i64 = length
            .try_into()
            .map_err(|e| io::Error::other(format!("Discard/write-zeroes length error: {e}")))?;

        let end = offset.saturating_add(length).saturating_add(1);
        let params = FILE_ZERO_DATA_INFORMATION {
            FileOffset: offset,
            BeyondFinalZero: end,
        };
        let mut _returned = 0;
        let file = self.file.read().unwrap();
        // Safe: File handle is valid, mandatory pointers (input, returned length) are passed and
        // valid, the parameter type matches the call, and the input size matches the object
        // passed.
        let ret = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_SET_ZERO_DATA,
                (&params as *const FILE_ZERO_DATA_INFORMATION).cast::<std::ffi::c_void>(),
                size_of_val(&params) as u32,
                std::ptr::null_mut(),
                0,
                &mut _returned,
                std::ptr::null_mut(),
            )
        };
        if ret == 0 {
            return Err(Self::map_os_err(io::Error::last_os_error()));
        }

        Ok(())
    }

    /// Via OS-specific means, ensure the given range reads back as zeroes, or return an error.
    #[cfg(target_os = "macos")]
    async fn discard_to_zero_os_specific(&self, offset: u64, length: u64) -> io::Result<()> {
        let offset: libc::off_t = offset
            .try_into()
            .map_err(|e| io::Error::other(format!("Discard/write-zeroes offset error: {e}")))?;
        let length: libc::off_t = length
            .try_into()
            .map_err(|e| io::Error::other(format!("Discard/write-zeroes length error: {e}")))?;

        let params = libc::fpunchhole_t {
            fp_flags: 0,
            reserved: 0,
            fp_offset: offset,
            fp_length: length,
        };
        let file = self.file.read().unwrap();
        // Safe: FD is valid, passed pointer is valid and its type matches the call.
        while_eintr(|| unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &params) })
            .map_err(Self::map_os_err)?;

        Ok(())
    }

    /// Via OS-specific means, ensure the given range reads back as zeroes, or return an error.
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    async fn discard_to_zero_os_specific(&self, offset: u64, length: u64) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

impl Display for File {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if let Some(filename) = self.filename.as_ref() {
            write!(f, "file:{filename:?}")
        } else {
            write!(f, "file:<unknown path>")
        }
    }
}

/// Convert a successful vectored-write result into observable forward progress.
#[cfg(unix)]
fn require_write_progress(length: libc::ssize_t) -> io::Result<u64> {
    if length == 0 {
        Err(io::ErrorKind::WriteZero.into())
    } else {
        debug_assert!(length > 0);
        Ok(length as u64)
    }
}

/// Turn a Linux vectored-write syscall return value into an I/O result.
#[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
fn syscall_result(result: libc::ssize_t) -> io::Result<libc::ssize_t> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

/// Retry an I/O operation only when it was interrupted before making observable progress.
#[cfg(any(
    all(target_os = "linux", any(target_env = "gnu", target_env = "musl")),
    all(test, unix)
))]
fn retry_interrupted<F>(mut operation: F) -> io::Result<libc::ssize_t>
where
    F: FnMut() -> io::Result<libc::ssize_t>,
{
    loop {
        match operation() {
            Err(error) if error.raw_os_error() == Some(libc::EINTR) => continue,
            result => return result,
        }
    }
}

/// Try a `RWF_DONTCACHE` write and fall back only when that hint is unsupported.
///
/// Both operations are supplied by the caller so an unsupported hinted write can be retried with
/// the ordinary syscall using the exact same unconsumed iovec and offset.  All other errors,
/// including `EINVAL`, remain visible to the caller rather than silently changing I/O semantics.
#[cfg(any(
    all(target_os = "linux", any(target_env = "gnu", target_env = "musl")),
    all(test, unix)
))]
fn write_with_optional_dontcache<H, P>(
    write_dontcache: &AtomicBool,
    hinted_write: H,
    plain_write: P,
) -> io::Result<libc::ssize_t>
where
    H: FnMut() -> io::Result<libc::ssize_t>,
    P: FnMut() -> io::Result<libc::ssize_t>,
{
    if !write_dontcache.load(Ordering::Relaxed) {
        return retry_interrupted(plain_write);
    }

    match retry_interrupted(hinted_write) {
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS)
            ) =>
        {
            // Kernel and filesystem support are authoritative.  Once either rejects the flag,
            // future writes avoid paying for another syscall that is known not to work.
            write_dontcache.store(false, Ordering::Relaxed);
            retry_interrupted(plain_write)
        }
        result => result,
    }
}

/// Get total size in bytes of the given file.
///
/// If the file is a block or character device, use get_device_size() instead of
/// reading len from metadata which doesn't work on some platforms like macOS.
fn get_file_size(file: &fs::File) -> io::Result<u64> {
    #[allow(clippy::bind_instead_of_map)]
    file.metadata().and_then(|m| {
        #[cfg(unix)]
        if m.file_type().is_block_device() || m.file_type().is_char_device() {
            return get_device_size(file);
        }
        Ok(m.len())
    })
}

cfg_if! {
    if #[cfg(target_os = "linux")] {
        /// Get total size in bytes of the given block or character device.
        fn get_device_size(file: &fs::File) -> io::Result<u64> {
            let mut size = 0;
            unsafe { ioctl::blkgetsize64(file.as_raw_fd(), &mut size) }?;
            Ok(size)
        }
    } else if #[cfg(target_os = "macos")] {
        /// Get total size in bytes of the given block or character device.
        fn get_device_size(file: &fs::File) -> io::Result<u64> {
            let mut block_size = 0;
            unsafe { ioctl::dkiocgetblocksize(file.as_raw_fd(), &mut block_size) }?;
            let mut block_count = 0;
            unsafe { ioctl::dkiocgetblockcount(file.as_raw_fd(), &mut block_count) }?;
            Ok(u64::from(block_size) * block_count)
        }
    } else if #[cfg(target_os = "freebsd")] {
        /// Get total size in bytes of the given block or character device.
        fn get_device_size(file: &fs::File) -> io::Result<u64> {
            let mut size = 0;
            unsafe { ioctl::diocgmediasize(file.as_raw_fd(), &mut size) }?;
            Ok(size as u64)
        }
    } else if #[cfg(unix)] {
        /// Get total size in bytes of the given block or character device - unsupported platform.
        fn get_device_size(_file: &fs::File) -> io::Result<u64> {
            Err(io::ErrorKind::Unsupported.into())
        }
    }
}

/// This module generates type-safe wrappers for chosen ioctls
mod ioctl {
    #[cfg(unix)]
    use nix::ioctl_read;
    #[cfg(target_os = "linux")]
    use nix::ioctl_read_bad;

    // https://github.com/torvalds/linux/blob/master/include/uapi/linux/fs.h#L200

    #[cfg(target_os = "linux")]
    ioctl_read!(blkgetsize64, 0x12, 114, u64);

    #[cfg(target_os = "linux")]
    ioctl_read_bad!(blksszget, libc::BLKSSZGET, libc::c_int);

    // https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/disk.h#L198-L199

    #[cfg(target_os = "macos")]
    ioctl_read!(dkiocgetblocksize, 'd', 24, u32);

    #[cfg(target_os = "macos")]
    ioctl_read!(dkiocgetblockcount, 'd', 25, u64);

    // https://web.mit.edu/freebsd/head/sys/sys/disk.h

    #[cfg(target_os = "freebsd")]
    ioctl_read!(diocgsectorsize, 'd', 128, libc::c_uint);

    #[cfg(target_os = "freebsd")]
    ioctl_read!(diocgmediasize, 'd', 129, libc::off_t);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    use std::cell::Cell;

    #[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
    #[test]
    fn write_dontcache_options_default_to_disabled() {
        let options = StorageOpenOptions::new();
        assert!(!options.get_write_dontcache());

        let options = options.write_dontcache(true);
        assert!(options.get_write_dontcache());
    }

    #[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
    #[test]
    fn write_dontcache_rejects_read_only_files() {
        let options = StorageOpenOptions::new().write_dontcache(true);
        let error = File::do_open_sync(options, fs::OpenOptions::new()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("writable storage"));
    }

    #[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
    #[test]
    fn write_dontcache_rejects_direct_io() {
        let options = StorageOpenOptions::new()
            .write(true)
            .direct(true)
            .write_dontcache(true);
        let error = File::do_open_sync(options, fs::OpenOptions::new()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("direct I/O"));
    }

    #[test]
    fn unsupported_hint_retries_the_same_tail_and_disables_future_hints() {
        for unsupported_error in [libc::EOPNOTSUPP, libc::ENOSYS] {
            let enabled = AtomicBool::new(true);
            let hinted_calls = Cell::new(0);
            let plain_calls = Cell::new(0);

            let length = write_with_optional_dontcache(
                &enabled,
                || {
                    hinted_calls.set(hinted_calls.get() + 1);
                    if hinted_calls.get() == 1 {
                        Err(io::Error::from_raw_os_error(libc::EINTR))
                    } else {
                        Err(io::Error::from_raw_os_error(unsupported_error))
                    }
                },
                || {
                    plain_calls.set(plain_calls.get() + 1);
                    Ok(37)
                },
            )
            .unwrap();

            assert_eq!(length, 37);
            assert_eq!(hinted_calls.get(), 2);
            assert_eq!(plain_calls.get(), 1);
            assert!(!enabled.load(Ordering::Relaxed));

            let length = write_with_optional_dontcache(
                &enabled,
                || panic!("a disabled hint must not be retried"),
                || {
                    plain_calls.set(plain_calls.get() + 1);
                    Ok(11)
                },
            )
            .unwrap();

            assert_eq!(length, 11);
            assert_eq!(plain_calls.get(), 2);
        }
    }

    #[test]
    fn invalid_hint_error_does_not_fall_back() {
        let enabled = AtomicBool::new(true);
        let plain_calls = Cell::new(0);

        let error = write_with_optional_dontcache(
            &enabled,
            || Err(io::Error::from_raw_os_error(libc::EINVAL)),
            || {
                plain_calls.set(plain_calls.get() + 1);
                Ok(1)
            },
        )
        .unwrap_err();

        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
        assert_eq!(plain_calls.get(), 0);
        assert!(enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn successful_partial_hint_write_advances_without_fallback() {
        let enabled = AtomicBool::new(true);
        let plain_calls = Cell::new(0);

        let length = write_with_optional_dontcache(
            &enabled,
            || Ok(13),
            || {
                plain_calls.set(plain_calls.get() + 1);
                Ok(99)
            },
        )
        .unwrap();

        assert_eq!(length, 13);
        assert_eq!(plain_calls.get(), 0);
        assert!(enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn zero_length_write_is_write_zero() {
        let error = require_write_progress(0).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WriteZero);
    }
}
