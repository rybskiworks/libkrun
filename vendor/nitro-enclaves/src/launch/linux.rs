// SPDX-License-Identifier: Apache-2.0

use super::{error::*, types::*};

use std::cmp::min;

use nix::{ioctl_read, ioctl_readwrite, ioctl_write_ptr};

pub const NE_MAGIC: u8 = 0xAE;

// Create a slot that is associated with an enclave VM.
pub const NE_CREATE_VM: u8 = 0x20;
ioctl_read!(ne_create_vm, NE_MAGIC, NE_CREATE_VM, u64);

// Set a vCPU for an enclave.
pub const NE_ADD_VCPU: u8 = 0x21;
ioctl_readwrite!(ne_add_vcpu, NE_MAGIC, NE_ADD_VCPU, u32);

// Get information needed for in-memory enclave image loading.
pub const NE_GET_IMAGE_LOAD_INFO: u8 = 0x22;
ioctl_readwrite!(
    ne_get_image_load_info,
    NE_MAGIC,
    NE_GET_IMAGE_LOAD_INFO,
    ImageLoadInfo
);

// Set an enclave's memory region.
pub const NE_SET_USER_MEMORY_REGION: u8 = 0x23;
ioctl_write_ptr!(
    ne_set_user_memory_region,
    NE_MAGIC,
    NE_SET_USER_MEMORY_REGION,
    UserMemoryRegion
);

// Start running an enclave.
pub const NE_START_ENCLAVE: u8 = 0x24;
ioctl_readwrite!(ne_start_enclave, NE_MAGIC, NE_START_ENCLAVE, StartInfo);

// Default enclave memory region.
const NE_DEFAULT_MEMORY_REGION: u64 = 0;

const HUGE_FLAG_SIZE: [(libc::c_int, usize); 9] = [
    (libc::MAP_HUGE_16GB, 16 << 30),
    (libc::MAP_HUGE_2GB, 2 << 30),
    (libc::MAP_HUGE_1GB, 1 << 30),
    (libc::MAP_HUGE_512MB, 512 << 20),
    (libc::MAP_HUGE_256MB, 256 << 20),
    (libc::MAP_HUGE_32MB, 32 << 20),
    (libc::MAP_HUGE_16MB, 16 << 20),
    (libc::MAP_HUGE_8MB, 8 << 20),
    (libc::MAP_HUGE_2MB, 2 << 20),
];

/// Info necessary for in-memory enclave image.
#[derive(Debug, Default)]
#[repr(C)]
pub struct ImageLoadInfo {
    /// Flags to determine the enclave image type (e.g. Enclave Image Format [EIF]).
    flags: u64,

    /// Offset in enclave memory where to start placing the enclave image.
    pub memory_offset: u64,
}

impl From<&ImageType<'_>> for ImageLoadInfo {
    fn from(image_type: &ImageType) -> Self {
        let flags = match image_type {
            ImageType::Eif(_) => 0x01,
        };

        Self {
            flags,
            ..Default::default()
        }
    }
}

/// Enclave memory region.
#[derive(Debug, Default)]
#[repr(C)]
pub struct UserMemoryRegion {
    /// Usage flags.
    pub flags: u64,

    /// Region size (in bytes).
    pub size: u64,

    /// Userspace (virtual) address of region.
    pub uaddr: u64,
}

impl UserMemoryRegion {
    pub fn image_fill(
        &mut self,
        image: &[u8],
        offset: usize,
        image_size: usize,
        written: &mut usize,
    ) -> Result<(), MemInitError> {
        let Some(location) = written.checked_add(self.size as usize) else {
            return Err(MemInitError::OffsetCheckOverflow);
        };

        if location > offset {
            // Calculate the offset both within the memory region and within the image file to
            // begin reading from and writing to.
            let region_offset = offset.saturating_sub(*written);
            let image_offset = written.saturating_sub(offset);

            // Amount to write is restricted by both the memory region's capacity and amount of
            // data remaining in the image file.
            let write_amount = min(
                self.size as usize - region_offset,
                image_size - image_offset,
            );

            let (region_start, region_end) = (region_offset, region_offset + write_amount);
            let (image_start, image_end) = (image_offset, image_offset + write_amount);

            let bytes = unsafe {
                std::slice::from_raw_parts_mut(self.uaddr as *mut u8, self.size as usize)
            };

            bytes[region_start..region_end].copy_from_slice(&image[image_start..image_end]);
        }

        *written += self.size as usize;

        Ok(())
    }
}

/// Allocated enclave memory regions.
pub struct UserMemoryRegions(Vec<UserMemoryRegion>);

impl UserMemoryRegions {
    /// Allocate huge pages for enclave memory from the requested size (in MiB).
    pub fn new(size_mib: usize) -> Result<Self, MemInitError> {
        let mut regions = Vec::new();
        let mut size = size_mib << 20;
        let mut found: bool;

        while size > 0 {
            found = false;
            for (hp_flag, reg_size) in HUGE_FLAG_SIZE {
                // Prevent wasting memory by only allocating huge pages that are smaller in size
                // than the remaining memory needing to be allocated.
                if size < reg_size {
                    continue;
                }

                let addr = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        reg_size,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB | hp_flag,
                        -1,
                        0,
                    )
                };

                if addr == libc::MAP_FAILED {
                    continue;
                }

                let region = UserMemoryRegion {
                    flags: NE_DEFAULT_MEMORY_REGION,
                    size: reg_size as _,
                    uaddr: addr as _,
                };

                regions.push(region);
                size -= reg_size;
                found = true;
            }

            // Ensure that a valid memory region was found and mapped. If not, return an error.
            if !found {
                return Err(MemInitError::NoHugePageFound);
            }
        }

        Ok(Self(regions))
    }

    /// Populate the memory regions with the enclave image.
    pub fn image_fill(&mut self, offset: usize, image: ImageType) -> Result<(), MemInitError> {
        // Only EIF images are supported at the moment.
        let ImageType::Eif(image) = image;

        // Get the size of the enclave image.
        let image_size = image.len();

        // Calculate the final index in guest memory in which the image should be written.
        let Some(limit) = offset.checked_add(image_size) else {
            return Err(MemInitError::ImagePlacementOverflow);
        };

        // Write the enclave image to the memory regions.
        let mut written: usize = 0;
        for region in &mut self.0 {
            region.image_fill(image, offset, image_size, &mut written)?;
            if written >= limit {
                break;
            }
        }

        // Ensure the entire enclave image was written.
        if written < limit {
            return Err(MemInitError::ImageWriteIncomplete);
        }

        Ok(())
    }

    /// Get a reference to the inner vector of memory regions.
    pub fn inner_ref(&self) -> &Vec<UserMemoryRegion> {
        &self.0
    }
}

/// Encapsulates info for startting an enclave.
#[repr(C)]
pub struct StartInfo {
    /// Usage flags.
    flags: u64,

    /// Enclave CID.
    pub cid: u64,
}

impl StartInfo {
    pub fn new(flags: StartFlags, cid: u64) -> Self {
        let flags = flags.bits();

        Self { flags, cid }
    }
}
