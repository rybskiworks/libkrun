// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

/// `VsockPacket` provides a thin wrapper over the buffers exchanged via virtio queues.
/// There are two components to a vsock packet, each using its own descriptor in a
/// virtio queue:
/// - the packet header; and
/// - the packet data/buffer.
///
/// There is a 1:1 relation between descriptor chains and packets: the first (chain head) holds
/// the header, and an optional second descriptor holds the data. The second descriptor is only
/// present for data packets (VSOCK_OP_RW).
///
/// TX headers are snapshotted before validation so guest writes cannot change
/// routing or lengths between checks. RX headers and payload buffers retain
/// direct guest-memory access to avoid copying packet data.
use std::convert::TryInto;
use std::ffi::CStr;
#[cfg(unix)]
use std::net::{Ipv4Addr, SocketAddrV4};
#[cfg(target_os = "macos")]
use std::net::{Ipv6Addr, SocketAddrV6};
use std::result;

#[cfg(target_os = "linux")]
use nix::sys::socket::{sockaddr, AddressFamily};
#[cfg(unix)]
use nix::sys::socket::{SockaddrLike, SockaddrStorage};
use utils::byte_order;
use vm_memory::{self, Address, Bytes, GuestAddress, GuestMemory, GuestMemoryError};

use super::super::DescriptorChain;
use super::defs;
use super::{Result, VsockError};

// The vsock packet header is defined by the C struct:
//
// ```C
// struct virtio_vsock_hdr {
//     le64 src_cid;
//     le64 dst_cid;
//     le32 src_port;
//     le32 dst_port;
//     le32 len;
//     le16 type;
//     le16 op;
//     le32 flags;
//     le32 buf_alloc;
//     le32 fwd_cnt;
// };
// ```
//
// This structed will occupy the buffer pointed to by the head descriptor. We'll be accessing it
// as a byte slice. To that end, we define below the offsets for each field struct, as well as the
// packed struct size, as a bunch of `usize` consts.
// Note that these offsets are only used privately by the `VsockPacket` struct, the public interface
// consisting of getter and setter methods, for each struct field, that will also handle the correct
// endianess.

/// The vsock packet header struct size (when packed).
pub const VSOCK_PKT_HDR_SIZE: usize = 44;

// Source CID.
const HDROFF_SRC_CID: usize = 0;

// Destination CID.
const HDROFF_DST_CID: usize = 8;

// Source port.
const HDROFF_SRC_PORT: usize = 16;

// Destination port.
const HDROFF_DST_PORT: usize = 20;

// Data length (in bytes) - may be 0, if there is no data buffer.
const HDROFF_LEN: usize = 24;

// Socket type. Currently, only connection-oriented streams are defined by the vsock protocol.
const HDROFF_TYPE: usize = 28;

// Operation ID - one of the VSOCK_OP_* values; e.g.
// - VSOCK_OP_RW: a data packet;
// - VSOCK_OP_REQUEST: connection request;
// - VSOCK_OP_RST: forcefull connection termination;
// etc (see `super::defs::uapi` for the full list).
const HDROFF_OP: usize = 30;

// Additional options (flags) associated with the current operation (`op`).
// Currently, only used with shutdown requests (VSOCK_OP_SHUTDOWN).
const HDROFF_FLAGS: usize = 32;

// Size (in bytes) of the packet sender receive buffer (for the connection to which this packet
// belongs).
const HDROFF_BUF_ALLOC: usize = 36;

// Number of bytes the sender has received and consumed (for the connection to which this packet
// belongs). For instance, for our Unix backend, this counter would be the total number of bytes
// we have successfully written to a backing Unix socket.
const HDROFF_FWD_CNT: usize = 40;

#[cfg(unix)]
#[repr(C)]
pub struct TsiProxyCreate {
    pub peer_port: u32,
    pub family: u16,
    pub _type: u16,
}

#[cfg(unix)]
#[repr(C)]
pub struct TsiConnectReq {
    pub peer_port: u32,
    pub addr: SockaddrStorage,
}

#[cfg(unix)]
#[repr(C)]
pub struct TsiConnectRsp {
    pub result: i32,
}

#[cfg(unix)]
#[repr(C)]
pub struct TsiGetnameReq {
    pub peer_port: u32,
    pub local_port: u32,
    pub peer: u32,
}

#[cfg(unix)]
#[repr(C)]
#[derive(Debug)]
pub struct TsiGetnameRsp {
    pub result: i32,
    pub addr_len: u32,
    pub addr: SockaddrStorage,
}

#[cfg(unix)]
impl Default for TsiGetnameRsp {
    fn default() -> Self {
        let addr: SockaddrStorage = SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0).into();
        TsiGetnameRsp {
            result: -1,
            // It's fine to unwrap here sice we've just created the SocketAddrV4 above.
            addr_len: addr.as_sockaddr_in().unwrap().len(),
            addr,
        }
    }
}

#[cfg(unix)]
#[repr(C)]
#[derive(Debug)]
pub struct TsiSendtoAddr {
    pub peer_port: u32,
    pub addr: SockaddrStorage,
}

#[cfg(unix)]
#[repr(C)]
#[derive(Debug)]
pub struct TsiListenReq {
    pub peer_port: u32,
    pub vm_port: u32,
    pub backlog: i32,
    pub addr: SockaddrStorage,
}

#[cfg(unix)]
#[repr(C)]
#[derive(Debug)]
pub struct TsiListenRsp {
    pub result: i32,
}

#[cfg(unix)]
#[repr(C)]
#[derive(Debug)]
pub struct TsiAcceptReq {
    pub peer_port: u32,
    pub flags: u32,
}

#[cfg(unix)]
#[repr(C)]
#[derive(Debug)]
pub struct TsiAcceptRsp {
    pub result: i32,
}

#[cfg(unix)]
#[repr(C)]
pub struct TsiReleaseReq {
    pub peer_port: u32,
    pub local_port: u32,
}

/// The vsock packet, implemented as a wrapper over a virtq descriptor chain:
/// - the chain head, holding the packet header; and
/// - (an optional) data/buffer descriptor, only present for data packets (VSOCK_OP_RW).
pub struct VsockPacket {
    hdr: PacketHeader,
    buf: Option<*mut u8>,
    buf_size: usize,
}

enum PacketHeader {
    Tx([u8; VSOCK_PKT_HDR_SIZE]),
    Rx(*mut u8),
}

fn get_host_address<T: GuestMemory + vm_memory::GuestMemoryBackend>(
    mem: &T,
    guest_addr: GuestAddress,
    size: usize,
) -> result::Result<*mut u8, GuestMemoryError> {
    Ok(mem.get_slice(guest_addr, size)?.ptr_guard_mut().as_ptr())
}

impl VsockPacket {
    /// Create the packet wrapper from a TX virtq chain head.
    ///
    /// The chain head is expected to hold valid packet header data. A following packet buffer
    /// descriptor can optionally end the chain. Bounds and pointer checks are performed when
    /// creating the wrapper.
    pub fn from_tx_virtq_head(head: &DescriptorChain) -> Result<Self> {
        // All buffers in the TX queue must be readable.
        //
        if head.is_write_only() {
            return Err(VsockError::UnreadableDescriptor);
        }

        // The packet header should fit inside the head descriptor.
        if head.len < VSOCK_PKT_HDR_SIZE as u32 {
            return Err(VsockError::HdrDescTooSmall(head.len));
        }

        let mut header = [0; VSOCK_PKT_HDR_SIZE];
        head.mem
            .read_slice(&mut header, head.addr)
            .map_err(VsockError::GuestMemoryMmap)?;
        let mut pkt = Self {
            hdr: PacketHeader::Tx(header),
            buf: None,
            buf_size: 0,
        };

        // No point looking for a data/buffer descriptor, if the packet is zero-lengthed.
        if pkt.len() == 0 {
            return Ok(pkt);
        }

        // Reject weirdly-sized packets.
        //
        if pkt.len() > defs::MAX_PKT_BUF_SIZE as u32 {
            return Err(VsockError::InvalidPktLen(pkt.len()));
        }

        // If the packet header showed a non-zero length, there should be a data descriptor here.
        let buf_desc = head.next_descriptor().ok_or(VsockError::BufDescMissing)?;

        // TX data should be read-only.
        if buf_desc.is_write_only() {
            return Err(VsockError::UnreadableDescriptor);
        }

        // The data buffer should be large enough to fit the size of the data, as described by
        // the header descriptor.
        if buf_desc.len < pkt.len() {
            return Err(VsockError::BufDescTooSmall);
        }

        pkt.buf_size = buf_desc.len as usize;
        pkt.buf = Some(
            get_host_address(buf_desc.mem, buf_desc.addr, pkt.buf_size)
                .map_err(VsockError::GuestMemoryMmap)?,
        );

        Ok(pkt)
    }

    /// Create the packet wrapper from an RX virtq chain head.
    ///
    /// There must be two descriptors in the chain, both writable: a header descriptor and a data
    /// descriptor. Bounds and pointer checks are performed when creating the wrapper.
    pub fn from_rx_virtq_head(head: &DescriptorChain) -> Result<Self> {
        // All RX buffers must be writable.
        //
        if !head.is_write_only() {
            return Err(VsockError::UnwritableDescriptor);
        }

        // The packet header should fit inside the head descriptor.
        if head.len < VSOCK_PKT_HDR_SIZE as u32 {
            return Err(VsockError::HdrDescTooSmall(head.len));
        }

        let mut pkt = Self {
            hdr: PacketHeader::Rx(
                get_host_address(head.mem, head.addr, VSOCK_PKT_HDR_SIZE)
                    .map_err(VsockError::GuestMemoryMmap)?,
            ),
            buf: None,
            buf_size: 0,
        };

        // Starting from Linux 6.2 the virtio-vsock driver can use a single descriptor for both
        // header and data.
        if !head.has_next() && head.len > VSOCK_PKT_HDR_SIZE as u32 {
            let buf_addr = head
                .addr
                .checked_add(VSOCK_PKT_HDR_SIZE as u64)
                .ok_or(VsockError::GuestMemoryBounds)?;

            pkt.buf_size = head.len as usize - VSOCK_PKT_HDR_SIZE;
            pkt.buf = Some(
                get_host_address(head.mem, buf_addr, pkt.buf_size)
                    .map_err(VsockError::GuestMemoryMmap)?,
            );
        } else {
            let buf_desc = head.next_descriptor().ok_or(VsockError::BufDescMissing)?;

            pkt.buf_size = buf_desc.len as usize;
            pkt.buf = Some(
                get_host_address(buf_desc.mem, buf_desc.addr, pkt.buf_size)
                    .map_err(VsockError::GuestMemoryMmap)?,
            );
        }

        Ok(pkt)
    }

    /// Read the TX snapshot or the in-place RX header.
    pub fn hdr(&self) -> &[u8] {
        match &self.hdr {
            PacketHeader::Tx(header) => header,
            // Bounds were checked when creating the packet from the descriptor.
            PacketHeader::Rx(ptr) => unsafe {
                std::slice::from_raw_parts(*ptr as *const u8, VSOCK_PKT_HDR_SIZE)
            },
        }
    }

    /// Modify the TX snapshot or write directly to the guest's RX header.
    pub fn hdr_mut(&mut self) -> &mut [u8] {
        match &mut self.hdr {
            PacketHeader::Tx(header) => header,
            // Bounds were checked when creating the packet from the descriptor.
            PacketHeader::Rx(ptr) => unsafe {
                std::slice::from_raw_parts_mut(*ptr, VSOCK_PKT_HDR_SIZE)
            },
        }
    }

    /// Provides in-place, byte-slice access to the vsock packet data buffer.
    ///
    /// Note: control packets (e.g. connection request or reset) have no data buffer associated.
    ///       For those packets, this method will return `None`.
    /// Also note: calling `len()` on the returned slice will yield the buffer size, which may be
    ///            (and often is) larger than the length of the packet data. The packet data length
    ///            is stored in the packet header, and accessible via `VsockPacket::len()`.
    pub fn buf(&self) -> Option<&[u8]> {
        self.buf.map(|ptr| {
            // This is safe since bound checks have already been performed when creating the packet
            // from the virtq descriptor.
            unsafe { std::slice::from_raw_parts(ptr as *const u8, self.buf_size) }
        })
    }

    /// Return exactly the data length declared in the packet header rather
    /// than the potentially larger backing descriptor.
    pub(crate) fn payload(&self) -> Option<&[u8]> {
        let len = self.len() as usize;
        if len == 0 {
            return Some(&[]);
        }
        self.buf()?.get(..len)
    }

    /// Provides in-place, byte-slice, mutable access to the vsock packet data buffer.
    ///
    /// Note: control packets (e.g. connection request or reset) have no data buffer associated.
    ///       For those packets, this method will return `None`.
    /// Also note: calling `len()` on the returned slice will yield the buffer size, which may be
    ///            (and often is) larger than the length of the packet data. The packet data length
    ///            is stored in the packet header, and accessible via `VsockPacket::len()`.
    pub fn buf_mut(&mut self) -> Option<&mut [u8]> {
        self.buf.map(|ptr| {
            // This is safe since bound checks have already been performed when creating the packet
            // from the virtq descriptor.
            unsafe { std::slice::from_raw_parts_mut(ptr, self.buf_size) }
        })
    }

    pub fn src_cid(&self) -> u64 {
        byte_order::read_le_u64(&self.hdr()[HDROFF_SRC_CID..])
    }

    pub fn set_src_cid(&mut self, cid: u64) -> &mut Self {
        byte_order::write_le_u64(&mut self.hdr_mut()[HDROFF_SRC_CID..], cid);
        self
    }

    pub fn dst_cid(&self) -> u64 {
        byte_order::read_le_u64(&self.hdr()[HDROFF_DST_CID..])
    }

    pub fn set_dst_cid(&mut self, cid: u64) -> &mut Self {
        byte_order::write_le_u64(&mut self.hdr_mut()[HDROFF_DST_CID..], cid);
        self
    }

    pub fn src_port(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_SRC_PORT..])
    }

    pub fn set_src_port(&mut self, port: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_SRC_PORT..], port);
        self
    }

    pub fn dst_port(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_DST_PORT..])
    }

    pub fn set_dst_port(&mut self, port: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_DST_PORT..], port);
        self
    }

    pub fn len(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_LEN..])
    }

    pub fn set_len(&mut self, len: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_LEN..], len);
        self
    }

    pub fn type_(&self) -> u16 {
        byte_order::read_le_u16(&self.hdr()[HDROFF_TYPE..])
    }

    pub fn set_type(&mut self, type_: u16) -> &mut Self {
        byte_order::write_le_u16(&mut self.hdr_mut()[HDROFF_TYPE..], type_);
        self
    }

    pub fn op(&self) -> u16 {
        byte_order::read_le_u16(&self.hdr()[HDROFF_OP..])
    }

    pub fn set_op(&mut self, op: u16) -> &mut Self {
        byte_order::write_le_u16(&mut self.hdr_mut()[HDROFF_OP..], op);
        self
    }

    pub fn flags(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_FLAGS..])
    }

    pub fn set_flags(&mut self, flags: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_FLAGS..], flags);
        self
    }

    pub fn set_flag(&mut self, flag: u32) -> &mut Self {
        self.set_flags(self.flags() | flag);
        self
    }

    pub fn buf_alloc(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_BUF_ALLOC..])
    }

    pub fn set_buf_alloc(&mut self, buf_alloc: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_BUF_ALLOC..], buf_alloc);
        self
    }

    pub fn fwd_cnt(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_FWD_CNT..])
    }

    pub fn set_fwd_cnt(&mut self, fwd_cnt: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_FWD_CNT..], fwd_cnt);
        self
    }

    pub fn sa_family(&self) -> Option<u16> {
        Some(byte_order::read_le_u16(self.payload()?.get(..2)?))
    }

    pub fn inet_port(&self) -> Option<u16> {
        Some(byte_order::read_be_u16(self.payload()?.get(2..4)?))
    }

    pub fn inet_addr(&self) -> Option<[u8; 4]> {
        self.payload()?.get(4..8)?.try_into().ok()
    }

    pub fn unix_path(&self) -> Option<&str> {
        CStr::from_bytes_until_nul(self.payload()?.get(2..108)?)
            .ok()?
            .to_str()
            .ok()
    }

    #[cfg(target_os = "linux")]
    fn parse_address(buf: &[u8], addr_len: u32) -> Option<SockaddrStorage> {
        let buf = buf.get(..usize::try_from(addr_len).ok()?)?;
        // The pinned nix implementation copies exactly addr_len bytes into
        // owned storage and checks the family-header/storage-size bounds.
        // Bound the source slice before passing the raw pointer to it.
        let sockaddr: SockaddrStorage =
            unsafe { SockaddrStorage::from_raw(buf.as_ptr() as *const sockaddr, Some(addr_len))? };

        match sockaddr.family() {
            Some(AddressFamily::Inet) if buf.len() == std::mem::size_of::<libc::sockaddr_in>() => {}
            Some(AddressFamily::Inet6)
                if buf.len() == std::mem::size_of::<libc::sockaddr_in6>() => {}
            Some(AddressFamily::Unix)
                if (2..=std::mem::size_of::<libc::sockaddr_un>()).contains(&buf.len()) => {}
            _ => {
                if let Some(family) = sockaddr.family() {
                    warn!("parse_address: unsupported family {family:?}");
                } else {
                    warn!("parse_address: error parsing family");
                }
                return None;
            }
        }

        Some(sockaddr)
    }

    #[cfg(target_os = "macos")]
    fn parse_address(buf: &[u8], addr_len: u32) -> Option<SockaddrStorage> {
        let buf = buf.get(..usize::try_from(addr_len).ok()?)?;
        let family: u16 = byte_order::read_le_u16(buf.get(..2)?);

        match family {
            defs::LINUX_AF_INET if buf.len() == 16 => {
                debug!("parse_address: AF_INET");
                let in_port: u16 = byte_order::read_be_u16(&buf[2..4]);
                let in_addr = Ipv4Addr::new(buf[4], buf[5], buf[6], buf[7]);
                Some(SocketAddrV4::new(in_addr, in_port).into())
            }
            defs::LINUX_AF_INET6 if buf.len() == 28 => {
                debug!("parse_address: AF_INET6");
                let in_port: u16 = byte_order::read_be_u16(&buf[2..4]);
                let flowinfo: u32 = byte_order::read_be_u32(&buf[4..8]);
                let in6_addr = Ipv6Addr::new(
                    byte_order::read_be_u16(&buf[8..10]),
                    byte_order::read_be_u16(&buf[10..12]),
                    byte_order::read_be_u16(&buf[12..14]),
                    byte_order::read_be_u16(&buf[14..16]),
                    byte_order::read_be_u16(&buf[16..18]),
                    byte_order::read_be_u16(&buf[18..20]),
                    byte_order::read_be_u16(&buf[20..22]),
                    byte_order::read_be_u16(&buf[22..24]),
                );
                let scope_id: u32 = byte_order::read_be_u32(&buf[24..28]);
                Some(SocketAddrV6::new(in6_addr, in_port, flowinfo, scope_id).into())
            }
            defs::LINUX_AF_UNIX => {
                // On macOS, SockaddrStorage doesn't implement `from_raw` for
                // Unix sockets, nor a way to cast an UnixPath to it.
                error!("AF_UNIX sockets aren't yet supported on macOS");
                None
            }
            _ => None,
        }
    }

    #[cfg(unix)]
    pub fn read_proxy_create(&self) -> Option<TsiProxyCreate> {
        let buf = self.payload()?.get(..8)?;
        Some(TsiProxyCreate {
            peer_port: byte_order::read_le_u32(&buf[0..4]),
            family: byte_order::read_le_u16(&buf[4..6]),
            _type: byte_order::read_le_u16(&buf[6..8]),
        })
    }

    #[cfg(unix)]
    pub fn read_connect_req(&self) -> Option<TsiConnectReq> {
        let buf = self.payload()?;
        let prefix = buf.get(..8)?;
        let peer_port = byte_order::read_le_u32(&prefix[0..4]);
        let addr_len = byte_order::read_le_u32(&prefix[4..8]);
        let addr = Self::parse_address(&buf[8..], addr_len)?;
        Some(TsiConnectReq { peer_port, addr })
    }

    #[cfg(unix)]
    pub fn write_connect_rsp(&mut self, rsp: TsiConnectRsp) {
        if self.buf_size >= 4 {
            if let Some(buf) = self.buf_mut() {
                byte_order::write_le_u32(&mut buf[0..], rsp.result as u32);
            }
        }
    }

    #[cfg(unix)]
    pub fn read_getname_req(&self) -> Option<TsiGetnameReq> {
        let buf = self.payload()?.get(..12)?;
        Some(TsiGetnameReq {
            peer_port: byte_order::read_le_u32(&buf[0..4]),
            local_port: byte_order::read_le_u32(&buf[4..8]),
            peer: byte_order::read_le_u32(&buf[8..12]),
        })
    }

    #[cfg(unix)]
    pub fn write_getname_rsp(&mut self, rsp: TsiGetnameRsp) {
        if self.buf_size >= 132 {
            if let Some(buf) = self.buf_mut() {
                byte_order::write_le_u32(&mut buf[0..], rsp.result as u32);
                byte_order::write_le_u32(&mut buf[4..], rsp.addr_len);
                let addr_ptr = rsp.addr.as_ptr();
                let slice = unsafe {
                    std::slice::from_raw_parts(addr_ptr as *const u8, rsp.addr.len() as usize)
                };
                buf[8..(rsp.addr.len() + 8) as usize].copy_from_slice(slice);

                // On macOS, convert BSD sockaddr (u8 sa_len + u8 sa_family) to
                // Linux wire format (u16 sa_family). Also translate macOS AF_*
                // values to their Linux equivalents (e.g. AF_INET6: 30 → 10).
                #[cfg(target_os = "macos")]
                {
                    let bsd_family = buf[9];
                    let linux_family: u16 = match bsd_family as i32 {
                        libc::AF_INET => defs::LINUX_AF_INET,
                        libc::AF_INET6 => defs::LINUX_AF_INET6,
                        _ => 0, // AF_UNSPEC
                    };
                    byte_order::write_le_u16(&mut buf[8..], linux_family);
                }
            }
        }
    }

    #[cfg(unix)]
    pub fn read_sendto_addr(&self) -> Option<TsiSendtoAddr> {
        let req = self.read_connect_req()?;
        Some(TsiSendtoAddr {
            peer_port: req.peer_port,
            addr: req.addr,
        })
    }

    #[cfg(unix)]
    pub fn read_listen_req(&self) -> Option<TsiListenReq> {
        let buf = self.payload()?;
        let prefix = buf.get(..16)?;
        let addr_len = byte_order::read_le_u32(&prefix[12..16]);
        Some(TsiListenReq {
            peer_port: byte_order::read_le_u32(&prefix[0..4]),
            vm_port: byte_order::read_le_u32(&prefix[4..8]),
            backlog: byte_order::read_le_u32(&prefix[8..12]) as i32,
            addr: Self::parse_address(&buf[16..], addr_len)?,
        })
    }

    #[cfg(unix)]
    pub fn write_listen_rsp(&mut self, rsp: TsiListenRsp) {
        if self.buf_size >= 4 {
            if let Some(buf) = self.buf_mut() {
                byte_order::write_le_u32(&mut buf[0..], rsp.result as u32);
            }
        }
    }

    #[cfg(unix)]
    pub fn read_accept_req(&self) -> Option<TsiAcceptReq> {
        let buf = self.payload()?.get(..8)?;
        Some(TsiAcceptReq {
            peer_port: byte_order::read_le_u32(&buf[0..4]),
            flags: byte_order::read_le_u32(&buf[4..8]),
        })
    }

    #[cfg(unix)]
    pub fn write_accept_rsp(&mut self, rsp: TsiAcceptRsp) {
        if self.buf_size >= 4 {
            if let Some(buf) = self.buf_mut() {
                byte_order::write_le_u32(&mut buf[0..], rsp.result as u32);
            }
        }
    }

    #[cfg(unix)]
    pub fn read_release_req(&self) -> Option<TsiReleaseReq> {
        let buf = self.payload()?.get(..8)?;
        Some(TsiReleaseReq {
            peer_port: byte_order::read_le_u32(&buf[0..4]),
            local_port: byte_order::read_le_u32(&buf[4..8]),
        })
    }

    pub fn write_time_sync(&mut self, time: u64) {
        if self.buf_size >= 8 {
            if let Some(buf) = self.buf_mut() {
                byte_order::write_le_u64(&mut buf[0..], time);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::Descriptor;
    use proptest::prelude::*;
    use vm_memory::GuestMemoryMmap;

    const TABLE: GuestAddress = GuestAddress(0x1000);
    const HEADER: GuestAddress = GuestAddress(0x2000);

    fn tx_payload(mem: &GuestMemoryMmap, bytes: &[u8], declared_len: usize) -> VsockPacket {
        mem.write_obj(
            Descriptor {
                addr: HEADER.0,
                len: VSOCK_PKT_HDR_SIZE as u32,
                flags: 1,
                next: 1,
            },
            TABLE,
        )
        .unwrap();
        mem.write_obj(
            Descriptor {
                addr: 0x3000,
                len: bytes.len() as u32,
                flags: 0,
                next: 0,
            },
            GuestAddress(TABLE.0 + 16),
        )
        .unwrap();
        mem.write_slice(&[0; VSOCK_PKT_HDR_SIZE], HEADER).unwrap();
        mem.write_obj(
            (declared_len as u32).to_le(),
            GuestAddress(HEADER.0 + HDROFF_LEN as u64),
        )
        .unwrap();
        mem.write_slice(bytes, GuestAddress(0x3000)).unwrap();
        let head = DescriptorChain::checked_new(mem, TABLE, 2, 0).unwrap();
        VsockPacket::from_tx_virtq_head(&head).unwrap()
    }

    proptest! {
        #[test]
        fn tx_identity_and_length_survive_arbitrary_guest_header_rewrites(
            source in any::<u64>(), destination in any::<u64>(),
            rewrite in any::<[u8; VSOCK_PKT_HDR_SIZE]>(),
            payload in proptest::collection::vec(any::<u8>(), 1..257),
            padding in proptest::collection::vec(any::<u8>(), 0..33),
        ) {
            let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
            let mut backing = payload.clone();
            backing.extend(padding);
            let mut pkt = tx_payload(&mem, &backing, payload.len());
            pkt.set_src_cid(source).set_dst_cid(destination);
            mem.write_slice(&rewrite, HEADER).unwrap();
            prop_assert_eq!(pkt.src_cid(), source);
            prop_assert_eq!(pkt.dst_cid(), destination);
            prop_assert_eq!(pkt.len() as usize, payload.len());
            prop_assert_eq!(pkt.payload(), Some(payload.as_slice()));
        }

        #[cfg(unix)]
        #[test]
        fn proxy_create_uses_only_complete_declared_payload(
            bytes in any::<[u8; 16]>(), declared_len in 0usize..=16,
        ) {
            let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
            let pkt = tx_payload(&mem, &bytes, declared_len);
            match pkt.read_proxy_create() {
                None => prop_assert!(declared_len < 8),
                Some(request) => {
                    prop_assert!(declared_len >= 8);
                    prop_assert_eq!(request.peer_port, u32::from_le_bytes(bytes[0..4].try_into().unwrap()));
                    prop_assert_eq!(request.family, u16::from_le_bytes(bytes[4..6].try_into().unwrap()));
                    prop_assert_eq!(request._type, u16::from_le_bytes(bytes[6..8].try_into().unwrap()));
                }
            }
        }

        #[cfg(unix)]
        #[test]
        fn control_readers_refuse_truncation_without_using_descriptor_padding(
            (bytes, declared_len) in proptest::collection::vec(any::<u8>(), 0..193)
                .prop_flat_map(|bytes| {
                    let len = bytes.len();
                    (Just(bytes), 0..=len)
                }),
        ) {
            let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
            let pkt = tx_payload(&mem, &bytes, declared_len);
            prop_assert_eq!(pkt.read_proxy_create().is_some(), declared_len >= 8);
            prop_assert_eq!(pkt.read_getname_req().is_some(), declared_len >= 12);
            prop_assert_eq!(pkt.read_accept_req().is_some(), declared_len >= 8);
            prop_assert_eq!(pkt.read_release_req().is_some(), declared_len >= 8);
            prop_assert_eq!(pkt.sa_family().is_some(), declared_len >= 2);
            prop_assert_eq!(pkt.inet_port().is_some(), declared_len >= 4);
            prop_assert_eq!(pkt.inet_addr().is_some(), declared_len >= 8);
            let _ = pkt.unix_path();
            let connect = pkt.read_connect_req();
            let sendto = pkt.read_sendto_addr();
            prop_assert_eq!(connect.is_some(), sendto.is_some());
            if connect.is_some() {
                prop_assert!(declared_len >= 10);
                let len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as u64;
                prop_assert!(len + 8 <= declared_len as u64);
            }
            if pkt.read_listen_req().is_some() {
                prop_assert!(declared_len >= 18);
                let len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as u64;
                prop_assert!(len + 16 <= declared_len as u64);
            }
        }

        #[cfg(unix)]
        #[test]
        fn valid_ipv4_control_addresses_retain_ports_and_reject_shortened_payloads(
            ip in any::<[u8; 4]>(), port in any::<u16>(), peer in any::<u32>(),
            missing in 1usize..=24,
        ) {
            let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
            let mut bytes = [0; 24];
            bytes[0..4].copy_from_slice(&peer.to_le_bytes());
            bytes[4..8].copy_from_slice(&16u32.to_le_bytes());
            bytes[8..10].copy_from_slice(&defs::LINUX_AF_INET.to_le_bytes());
            bytes[10..12].copy_from_slice(&port.to_be_bytes());
            bytes[12..16].copy_from_slice(&ip);
            let pkt = tx_payload(&mem, &bytes, bytes.len());
            let request = pkt.read_connect_req().unwrap();
            prop_assert_eq!(request.peer_port, peer);
            let addr = request.addr.as_sockaddr_in().unwrap();
            prop_assert_eq!(addr.ip().octets(), ip);
            prop_assert_eq!(addr.port(), port);
            prop_assert!(pkt.read_sendto_addr().is_some());
            let truncated = tx_payload(&mem, &bytes, bytes.len() - missing);
            prop_assert!(truncated.read_connect_req().is_none());
            prop_assert!(truncated.read_sendto_addr().is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn minimized_proxy_create_padding_and_short_buffer_regressions() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        // A generated one-byte request formerly decoded the descriptor tail
        // as a complete command. Six/seven-byte buffers also indexed past it.
        for (bytes, len) in [(&[0; 16][..], 1), (&[0; 6][..], 6), (&[0; 7][..], 7)] {
            assert!(tx_payload(&mem, bytes, len).read_proxy_create().is_none());
        }
        let bytes = [0xff; 108];
        assert!(tx_payload(&mem, &bytes, bytes.len()).unix_path().is_none());
    }

    fn descriptor(mem: &GuestMemoryMmap, writable: bool) -> DescriptorChain<'_> {
        mem.write_obj(
            Descriptor {
                addr: HEADER.0,
                len: VSOCK_PKT_HDR_SIZE as u32 + 8,
                flags: if writable { 2 } else { 0 },
                next: 0,
            },
            TABLE,
        )
        .unwrap();
        DescriptorChain::checked_new(mem, TABLE, 1, 0).unwrap()
    }

    #[test]
    fn tx_header_is_stable_after_guest_memory_changes() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let head = descriptor(&mem, false);
        let pkt = VsockPacket::from_tx_virtq_head(&head).unwrap();

        mem.write_slice(&[0xff; VSOCK_PKT_HDR_SIZE], HEADER)
            .unwrap();

        assert_eq!(pkt.hdr(), &[0; VSOCK_PKT_HDR_SIZE]);
        assert_eq!(pkt.len(), 0);
        assert_eq!(pkt.payload(), Some(&[][..]));
    }

    #[test]
    fn rx_header_still_writes_into_guest_memory() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let head = descriptor(&mem, true);
        let mut pkt = VsockPacket::from_rx_virtq_head(&head).unwrap();

        pkt.set_src_cid(2).set_dst_cid(42).set_dst_port(5000);

        let mut written = [0; VSOCK_PKT_HDR_SIZE];
        mem.read_slice(&mut written, HEADER).unwrap();
        assert_eq!(written.as_slice(), pkt.hdr());
        assert_eq!(byte_order::read_le_u64(&written[HDROFF_DST_CID..]), 42);
        assert_eq!(byte_order::read_le_u32(&written[HDROFF_DST_PORT..]), 5000);
    }
}
