//! Macro to allow binary structures be read/stored from/to disk.

use std::io;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// A structure to be loaded from or stored on disk (e.g. qcow2 image header), in binary format.
pub(crate) trait OnDiskStruct: Sized {
    /// The on-disk size of this structure, in bytes
    const ON_DISK_SIZE: usize;

    /// Convenience alias for [`Self::ON_DISK_SIZE`] for objects
    fn on_disk_size(&self) -> usize {
        Self::ON_DISK_SIZE
    }

    /// Load this structure from the given slice.
    ///
    /// Use little endian, unless the structure has a specific inherent endianness.
    #[allow(dead_code)]
    fn load_from_le(bytes: &[u8]) -> io::Result<Self> {
        Self::load_from(bytes)
    }

    /// Store this structure in the given slice.
    ///
    /// Use little endian, unless the structure has a specific inherent endianness.
    #[allow(dead_code)]
    fn store_to_le(&self, bytes: &mut [u8]) -> io::Result<()> {
        self.store_to(bytes)
    }

    /// Load this structure from the given slice.
    ///
    /// Use big endian, unless the structure has a specific inherent endianness.
    fn load_from_be(bytes: &[u8]) -> io::Result<Self> {
        Self::load_from(bytes)
    }

    /// Store this structure in the given slice.
    ///
    /// Use big endian, unless the structure has a specific inherent endianness.
    fn store_to_be(&self, bytes: &mut [u8]) -> io::Result<()> {
        self.store_to(bytes)
    }

    /// Load this structure from the given slice, in default inherent endianness.
    fn load_from(bytes: &[u8]) -> io::Result<Self>;

    /// Store this structure in the given slice, in default inherent endianness.
    fn store_to(&self, bytes: &mut [u8]) -> io::Result<()>;
}

/// Implement [`OnDiskStruct`] for a type wrapping a primitive type.
///
/// First argument: The wrapping type, e.g. `AtomicU16`; second argument: The inner primitive type
/// (e.g. the unstable `AtomicPrimitive::Storage` associated type); third
/// argument: The wrapping function (for loading); fourth argument: The unwrapping function (for
/// storing).
macro_rules! impl_on_disk_struct_wrapped_primitive {
    ($type:ty, $raw_type:ty, $wrap:expr, $unwrap:expr) => {
        impl $crate::macros::on_disk_struct::OnDiskStruct for $type {
            const ON_DISK_SIZE: usize = std::mem::size_of::<$raw_type>();

            fn load_from_le(bytes: &[u8]) -> std::io::Result<Self> {
                Ok($wrap(<$raw_type>::load_from_le(bytes)?))
            }

            fn store_to_le(&self, bytes: &mut [u8]) -> std::io::Result<()> {
                let raw = $unwrap(self);
                raw.store_to_le(bytes)
            }

            fn load_from_be(bytes: &[u8]) -> std::io::Result<Self> {
                Ok($wrap(<$raw_type>::load_from_be(bytes)?))
            }

            fn store_to_be(&self, bytes: &mut [u8]) -> std::io::Result<()> {
                let raw = $unwrap(self);
                raw.store_to_be(bytes)
            }

            fn load_from(bytes: &[u8]) -> std::io::Result<Self> {
                Ok($wrap(<$raw_type>::load_from(bytes)?))
            }

            fn store_to(&self, bytes: &mut [u8]) -> std::io::Result<()> {
                let raw = $unwrap(self);
                raw.store_to(bytes)
            }
        }
    };
}

/// Implement [`OnDiskStruct`] for a primitive integer type.
macro_rules! impl_on_disk_struct_primitive {
    ($type:ty) => {
        impl $crate::macros::on_disk_struct::OnDiskStruct for $type {
            const ON_DISK_SIZE: usize = std::mem::size_of::<$type>();

            fn load_from_le(bytes: &[u8]) -> std::io::Result<Self> {
                Ok(<$type>::from_le_bytes(bytes.try_into().map_err(|_| {
                    $crate::misc_helpers::invalid_data(format!(
                        "Cannot load type {} (length {}) from buffer of length {}",
                        std::any::type_name::<$type>(),
                        Self::ON_DISK_SIZE,
                        bytes.len(),
                    ))
                })?))
            }

            fn store_to_le(&self, bytes: &mut [u8]) -> std::io::Result<()> {
                if bytes.len() != Self::ON_DISK_SIZE {
                    return Err($crate::misc_helpers::invalid_data(format!(
                        "Cannot write type {} (length {}) into buffer of length {}",
                        std::any::type_name::<$type>(),
                        Self::ON_DISK_SIZE,
                        bytes.len(),
                    )));
                }

                bytes.copy_from_slice(&self.to_le_bytes());
                Ok(())
            }

            fn load_from_be(bytes: &[u8]) -> std::io::Result<Self> {
                Ok(<$type>::from_be_bytes(bytes.try_into().map_err(|_| {
                    $crate::misc_helpers::invalid_data(format!(
                        "Cannot load type {} (length {}) from buffer of length {}",
                        std::any::type_name::<$type>(),
                        Self::ON_DISK_SIZE,
                        bytes.len(),
                    ))
                })?))
            }

            fn store_to_be(&self, bytes: &mut [u8]) -> std::io::Result<()> {
                if bytes.len() != Self::ON_DISK_SIZE {
                    return Err($crate::misc_helpers::invalid_data(format!(
                        "Cannot write type {} (length {}) into buffer of length {}",
                        std::any::type_name::<$type>(),
                        Self::ON_DISK_SIZE,
                        bytes.len(),
                    )));
                }

                bytes.copy_from_slice(&self.to_be_bytes());
                Ok(())
            }

            fn load_from(bytes: &[u8]) -> std::io::Result<Self> {
                Ok(<$type>::from_ne_bytes(bytes.try_into().map_err(|_| {
                    $crate::misc_helpers::invalid_data(format!(
                        "Cannot load type {} (length {}) from buffer of length {}",
                        std::any::type_name::<$type>(),
                        Self::ON_DISK_SIZE,
                        bytes.len(),
                    ))
                })?))
            }

            fn store_to(&self, bytes: &mut [u8]) -> std::io::Result<()> {
                if bytes.len() != Self::ON_DISK_SIZE {
                    return Err($crate::misc_helpers::invalid_data(format!(
                        "Cannot write type {} (length {}) into buffer of length {}",
                        std::any::type_name::<$type>(),
                        Self::ON_DISK_SIZE,
                        bytes.len(),
                    )));
                }

                bytes.copy_from_slice(&self.to_ne_bytes());
                Ok(())
            }
        }
    };
}

impl_on_disk_struct_primitive!(u16);
impl_on_disk_struct_primitive!(u32);
impl_on_disk_struct_primitive!(u64);
impl_on_disk_struct_wrapped_primitive!(AtomicU32, u32, Into::into, |x: &AtomicU32| x
    .load(Ordering::Relaxed));
impl_on_disk_struct_wrapped_primitive!(AtomicU64, u64, Into::into, |x: &AtomicU64| x
    .load(Ordering::Relaxed));

/// Implement [`OnDiskStruct`] for the contained `struct` definition.
///
/// The struct name must be followed by a specification of endianness and whether to allow gaps in
/// the field offsets, i.e.: `struct <Struct>/<LE, BE, NE>, <no_gaps, allow_gaps>`.
///
/// All field types must be annotated with their byte offset in the structure, e.g. `foo: u32[42]`.
macro_rules! on_disk_struct {
    (
        $(#[$attr:meta])*
        struct $struct_name:ident/$endianness:ident, $packed:ident {
            $(
                $(#[$id_attr:meta])*
                $identifier:ident: $type:ty[$offset:literal],
            )+
        }
    ) => {
        $(#[$attr])*
        struct $struct_name {
            $(
                $(#[$id_attr])*
                $identifier: $type,
            )+
        }

        // Verify strict field ordering
        const _: () = const {
            let mut next = 0;
            $(
                $crate::macros::on_disk_struct::on_disk_struct_helper!(@check_layout $packed, $offset, next);
                next = $offset + <$type>::ON_DISK_SIZE;
            )+
            let _ = next;
        };

        impl $crate::macros::on_disk_struct::OnDiskStruct for $struct_name {
            const ON_DISK_SIZE: usize = $crate::macros::last_element!($($offset + <$type>::ON_DISK_SIZE),+);

            fn load_from(bytes: &[u8]) -> std::io::Result<Self> {
                if bytes.len() < Self::ON_DISK_SIZE {
                    return Err($crate::misc_helpers::invalid_data(format!(
                        "Cannot read struct {} (length {}) from buffer of length {}",
                        std::any::type_name::<$struct_name>(),
                        Self::ON_DISK_SIZE,
                        bytes.len(),
                    )));
                }

                Ok($struct_name {
                    $(
                        $identifier: $crate::macros::on_disk_struct::on_disk_struct_helper!(
                            @load $endianness,
                            $type,
                            &bytes[$offset..($offset + <$type>::ON_DISK_SIZE)]
                        )?,
                    )+
                })
            }

            fn store_to(&self, bytes: &mut [u8]) -> std::io::Result<()> {
                if bytes.len() < Self::ON_DISK_SIZE {
                    return Err($crate::misc_helpers::invalid_data(format!(
                        "Cannot write struct {} (length {}) into buffer of length {}",
                        std::any::type_name::<$struct_name>(),
                        Self::ON_DISK_SIZE,
                        bytes.len(),
                    )));
                }

                $(
                    $crate::macros::on_disk_struct::on_disk_struct_helper!(
                        @store $endianness,
                        self.$identifier,
                        &mut bytes[$offset..($offset + <$type>::ON_DISK_SIZE)]
                    )?;
                )+

                Ok(())
            }
        }
    }
}

pub(crate) use on_disk_struct;

/// Helper macro for [`on_disk_struct`].
///
/// Various functionalities, selected by the first parameter.
macro_rules! on_disk_struct_helper {
    // Check a field offset against the actual offset within the struct, either allowing gaps or not
    (@check_layout no_gaps, $field_offset:literal, $actual_offset:expr) => {
        assert!($field_offset == $actual_offset);
    };
    (@check_layout allow_gaps, $field_offset:literal, $actual_offset:expr) => {
        assert!($field_offset >= $actual_offset);
    };

    // Load with an endianness specified
    (@load NE, $type:ty, $slice:expr) => {
        <$type>::load_from($slice)
    };
    (@load LE, $type:ty, $slice:expr) => {
        <$type>::load_from_le($slice)
    };
    (@load BE, $type:ty, $slice:expr) => {
        <$type>::load_from_be($slice)
    };

    // Store with an endianness specified
    (@store NE, $value:expr, $slice:expr) => {
        $value.store_to($slice)
    };
    (@store LE, $value:expr, $slice:expr) => {
        $value.store_to_le($slice)
    };
    (@store BE, $value:expr, $slice:expr) => {
        $value.store_to_be($slice)
    };
}

pub(crate) use on_disk_struct_helper;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    // -- Primitive round-trips --

    #[test]
    fn u16_be_round_trip() {
        let val: u16 = 0x0102;
        let mut buf = [0u8; 2];
        val.store_to_be(&mut buf).unwrap();
        assert_eq!(buf, [0x01, 0x02]);
        assert_eq!(u16::load_from_be(&buf).unwrap(), val);
    }

    #[test]
    fn u16_le_round_trip() {
        let val: u16 = 0x0102;
        let mut buf = [0u8; 2];
        val.store_to_le(&mut buf).unwrap();
        assert_eq!(buf, [0x02, 0x01]);
        assert_eq!(u16::load_from_le(&buf).unwrap(), val);
    }

    #[test]
    fn u32_be_round_trip() {
        let val: u32 = 0x01020304;
        let mut buf = [0u8; 4];
        val.store_to_be(&mut buf).unwrap();
        assert_eq!(buf, [0x01, 0x02, 0x03, 0x04]);
        assert_eq!(u32::load_from_be(&buf).unwrap(), val);
    }

    #[test]
    fn u32_le_round_trip() {
        let val: u32 = 0x01020304;
        let mut buf = [0u8; 4];
        val.store_to_le(&mut buf).unwrap();
        assert_eq!(buf, [0x04, 0x03, 0x02, 0x01]);
        assert_eq!(u32::load_from_le(&buf).unwrap(), val);
    }

    #[test]
    fn u64_be_round_trip() {
        let val: u64 = 0x0102030405060708;
        let mut buf = [0u8; 8];
        val.store_to_be(&mut buf).unwrap();
        assert_eq!(buf, [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
        assert_eq!(u64::load_from_be(&buf).unwrap(), val);
    }

    #[test]
    fn u64_le_round_trip() {
        let val: u64 = 0x0102030405060708;
        let mut buf = [0u8; 8];
        val.store_to_le(&mut buf).unwrap();
        assert_eq!(buf, [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        assert_eq!(u64::load_from_le(&buf).unwrap(), val);
    }

    // -- Primitive error cases --

    #[test]
    fn primitive_store_wrong_size() {
        let val: u32 = 42;
        assert!(val.store_to_be(&mut [0u8; 3]).is_err());
        assert!(val.store_to_be(&mut [0u8; 5]).is_err());
        assert!(val.store_to_le(&mut [0u8; 0]).is_err());
    }

    #[test]
    fn primitive_load_wrong_size() {
        assert!(u32::load_from_be(&[0u8; 3]).is_err());
        assert!(u32::load_from_be(&[0u8; 5]).is_err());
        assert!(u64::load_from_le(&[0u8; 7]).is_err());
    }

    // -- Atomic round-trips --

    #[test]
    fn atomic_u32_be_round_trip() {
        let val = AtomicU32::new(0xdeadbeef);
        let mut buf = [0u8; 4];
        val.store_to_be(&mut buf).unwrap();
        assert_eq!(buf, [0xde, 0xad, 0xbe, 0xef]);
        let loaded = AtomicU32::load_from_be(&buf).unwrap();
        assert_eq!(loaded.load(Ordering::Relaxed), 0xdeadbeef);
    }

    #[test]
    fn atomic_u64_le_round_trip() {
        let val = AtomicU64::new(0x0102030405060708);
        let mut buf = [0u8; 8];
        val.store_to_le(&mut buf).unwrap();
        assert_eq!(buf, [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        let loaded = AtomicU64::load_from_le(&buf).unwrap();
        assert_eq!(loaded.load(Ordering::Relaxed), 0x0102030405060708);
    }

    // -- on_disk_struct: big-endian, no gaps --

    on_disk_struct! {
        struct TestBE/BE, no_gaps {
            a: u32[0],
            b: u64[4],
            c: u16[12],
        }
    }

    #[test]
    fn be_struct_on_disk_size() {
        assert_eq!(TestBE::ON_DISK_SIZE, 14);
    }

    #[test]
    fn be_struct_round_trip() {
        let s = TestBE {
            a: 0x01020304,
            b: 0x0506070809101112,
            c: 0x1314,
        };
        let mut buf = [0u8; 14];
        s.store_to(&mut buf).unwrap();

        // Verify big-endian byte layout
        assert_eq!(&buf[0..4], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(
            &buf[4..12],
            &[0x05, 0x06, 0x07, 0x08, 0x09, 0x10, 0x11, 0x12]
        );
        assert_eq!(&buf[12..14], &[0x13, 0x14]);

        let loaded = TestBE::load_from(&buf).unwrap();
        assert_eq!(loaded.a, s.a);
        assert_eq!(loaded.b, s.b);
        assert_eq!(loaded.c, s.c);
    }

    #[test]
    fn be_struct_load_from_larger_buffer() {
        let mut buf = [0u8; 20];
        buf[0..4].copy_from_slice(&0x11223344u32.to_be_bytes());
        let loaded = TestBE::load_from(&buf).unwrap();
        assert_eq!(loaded.a, 0x11223344);
    }

    #[test]
    fn be_struct_load_too_short() {
        assert!(TestBE::load_from(&[0u8; 13]).is_err());
        assert!(TestBE::load_from(&[0u8; 0]).is_err());
    }

    #[test]
    fn be_struct_store_too_short() {
        let s = TestBE { a: 0, b: 0, c: 0 };
        assert!(s.store_to(&mut [0u8; 13]).is_err());
    }

    // -- on_disk_struct: little-endian, no gaps --

    on_disk_struct! {
        struct TestLE/LE, no_gaps {
            x: u32[0],
            y: u32[4],
        }
    }

    #[test]
    fn le_struct_round_trip() {
        let s = TestLE {
            x: 0x01020304,
            y: 0x05060708,
        };
        let mut buf = [0u8; 8];
        s.store_to(&mut buf).unwrap();

        // Verify little-endian byte layout
        assert_eq!(&buf[0..4], &[0x04, 0x03, 0x02, 0x01]);
        assert_eq!(&buf[4..8], &[0x08, 0x07, 0x06, 0x05]);

        let loaded = TestLE::load_from(&buf).unwrap();
        assert_eq!(loaded.x, s.x);
        assert_eq!(loaded.y, s.y);
    }

    // -- on_disk_struct: with atomics --

    on_disk_struct! {
        struct TestAtomics/BE, no_gaps {
            plain: u32[0],
            atomic32: AtomicU32[4],
            atomic64: AtomicU64[8],
        }
    }

    #[test]
    fn atomic_struct_on_disk_size() {
        assert_eq!(TestAtomics::ON_DISK_SIZE, 16);
    }

    #[test]
    fn atomic_struct_round_trip() {
        let s = TestAtomics {
            plain: 0xaaaaaaaa,
            atomic32: AtomicU32::new(0xbbbbbbbb),
            atomic64: AtomicU64::new(0xccccccccdddddddd),
        };
        let mut buf = [0u8; 16];
        s.store_to(&mut buf).unwrap();

        assert_eq!(&buf[0..4], &[0xaa, 0xaa, 0xaa, 0xaa]);
        assert_eq!(&buf[4..8], &[0xbb, 0xbb, 0xbb, 0xbb]);
        assert_eq!(
            &buf[8..16],
            &[0xcc, 0xcc, 0xcc, 0xcc, 0xdd, 0xdd, 0xdd, 0xdd]
        );

        let loaded = TestAtomics::load_from(&buf).unwrap();
        assert_eq!(loaded.plain, 0xaaaaaaaa);
        assert_eq!(loaded.atomic32.load(Ordering::Relaxed), 0xbbbbbbbb);
        assert_eq!(loaded.atomic64.load(Ordering::Relaxed), 0xccccccccdddddddd);
    }

    // -- on_disk_struct: allow_gaps --

    on_disk_struct! {
        struct TestGaps/BE, allow_gaps {
            first: u32[0],
            // 4-byte gap at offset 4
            second: u16[8],
        }
    }

    #[test]
    fn gaps_struct_on_disk_size() {
        assert_eq!(TestGaps::ON_DISK_SIZE, 10);
    }

    #[test]
    fn gaps_struct_round_trip() {
        let s = TestGaps {
            first: 0x11223344,
            second: 0x5566,
        };
        let mut buf = [0xffu8; 10];
        s.store_to(&mut buf).unwrap();

        assert_eq!(&buf[0..4], &[0x11, 0x22, 0x33, 0x44]);
        // Gap bytes at [4..8] are untouched
        assert_eq!(&buf[4..8], &[0xff, 0xff, 0xff, 0xff]);
        assert_eq!(&buf[8..10], &[0x55, 0x66]);

        // Round-trip ignores gap bytes
        let loaded = TestGaps::load_from(&buf).unwrap();
        assert_eq!(loaded.first, 0x11223344);
        assert_eq!(loaded.second, 0x5566);
    }

    // -- on_disk_struct: single field --

    on_disk_struct! {
        struct TestSingle/BE, no_gaps {
            only: u64[0],
        }
    }

    #[test]
    fn single_field_struct() {
        assert_eq!(TestSingle::ON_DISK_SIZE, 8);
        let s = TestSingle {
            only: 0x0102030405060708,
        };
        let mut buf = [0u8; 8];
        s.store_to(&mut buf).unwrap();
        assert_eq!(buf, [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
        let loaded = TestSingle::load_from(&buf).unwrap();
        assert_eq!(loaded.only, s.only);
    }

    // -- on_disk_struct: inherent endianness overrides le/be calls --

    #[test]
    fn be_struct_inherent_endianness() {
        let s = TestBE {
            a: 0x01020304,
            b: 0,
            c: 0,
        };
        let mut buf_via_store = [0u8; 14];
        let mut buf_via_be = [0u8; 14];
        let mut buf_via_le = [0u8; 14];

        s.store_to(&mut buf_via_store).unwrap();
        s.store_to_be(&mut buf_via_be).unwrap();
        s.store_to_le(&mut buf_via_le).unwrap();

        // All three should produce identical output (inherent BE endianness)
        assert_eq!(buf_via_store, buf_via_be);
        assert_eq!(buf_via_store, buf_via_le);
    }

    // -- on_disk_size convenience method --

    #[test]
    fn on_disk_size_method() {
        let s = TestBE { a: 0, b: 0, c: 0 };
        assert_eq!(s.on_disk_size(), TestBE::ON_DISK_SIZE);
        assert_eq!(s.on_disk_size(), 14);
    }

    // -- Known byte pattern: decode hand-crafted bytes --

    #[test]
    fn decode_known_be_bytes() {
        // Hand-crafted big-endian buffer
        let buf: [u8; 14] = [
            0x00, 0x00, 0x00, 0x01, // a = 1
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, // b = 255
            0x01, 0x00, // c = 256
        ];
        let s = TestBE::load_from(&buf).unwrap();
        assert_eq!(s.a, 1);
        assert_eq!(s.b, 255);
        assert_eq!(s.c, 256);
    }
}
