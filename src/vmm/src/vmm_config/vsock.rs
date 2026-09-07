// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(not(target_os = "windows"))]
use devices::virtio::vsock::VsockDatagramPortBackend;
use devices::virtio::vsock::VsockPortBackend;
pub use devices::virtio::TsiFlags;
use devices::virtio::{Vsock, VsockError};

type MutexVsock = Arc<Mutex<Vsock>>;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

#[cfg(not(target_os = "windows"))]
const VSOCK_TIMESYNC_PORT: u32 = 123;
#[cfg(not(target_os = "windows"))]
const TSI_CONTROL_PORT_START: u32 = 1024;
#[cfg(not(target_os = "windows"))]
const TSI_CONTROL_PORT_END: u32 = 1031;

/// Errors associated with `NetworkInterfaceConfig`.
#[derive(Debug)]
pub enum VsockConfigError {
    /// Failed to create the vsock device.
    CreateVsockDevice(VsockError),
    /// A custom datagram route overlaps a device-owned protocol port.
    ReservedDatagramPort { port: u32, owner: &'static str },
    /// A guest CID was explicitly requested that is reserved for the host.
    InvalidGuestCid(u32),
    /// A guest CID was explicitly requested that is already assigned to another VM.
    GuestCidInUse(u32),
    /// The process-global guest CID space is exhausted.
    GuestCidExhausted,
}

impl fmt::Display for VsockConfigError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::VsockConfigError::*;
        match *self {
            CreateVsockDevice(ref e) => write!(f, "Cannot create vsock device: {e:?}"),
            ReservedDatagramPort { port, owner } => {
                write!(
                    f,
                    "Cannot route vsock datagram port {port}: reserved for {owner}"
                )
            }
            InvalidGuestCid(cid) => write!(
                f,
                "Invalid guest CID {cid}: CIDs 0, 1 and 2 are reserved (host CID is 2)"
            ),
            GuestCidInUse(cid) => {
                write!(f, "Guest CID {cid} is already assigned to another VM")
            }
            GuestCidExhausted => write!(f, "No unassigned guest CID remains"),
        }
    }
}

type Result<T> = std::result::Result<T, VsockConfigError>;

//--------------------------------------------------------------------------------------------------
// Guest CID allocation
//--------------------------------------------------------------------------------------------------

/// First assignable guest CID.
///
/// CIDs 0 and 1 are reserved by the vsock specification and CID 2 addresses
/// the host (`VSOCK_HOST_CID`). Allocation starts at 3, preserving the legacy
/// single-VM behavior for the first VM in a process.
pub const FIRST_GUEST_CID: u32 = 3;

/// Next candidate CID handed out by [`allocate_guest_cid`].
///
/// Monotonically increasing; wasted values (explicitly reserved CIDs that the
/// counter steps over, or CIDs allocated for contexts whose vsock ends up
/// disabled) are never reused. The space is effectively inexhaustible, but
/// exhaustion fails closed (see [`VsockConfigError::GuestCidExhausted`]).
static NEXT_GUEST_CID: AtomicU32 = AtomicU32::new(FIRST_GUEST_CID);

/// Every CID currently owned by a VM in this process, whether auto-allocated
/// or explicitly reserved via [`reserve_guest_cid`].
static USED_GUEST_CIDS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();

fn used_guest_cids() -> &'static Mutex<HashSet<u32>> {
    USED_GUEST_CIDS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Allocate a process-unique guest CID.
///
/// Returns CIDs starting at [`FIRST_GUEST_CID`] (3), monotonically increasing,
/// and never returns 0, 1 or 2. CIDs pinned beforehand with
/// [`reserve_guest_cid`] are skipped, never handed out twice. Shared by the
/// Rust (`msb_krun`) and C (`libkrun`) VM-creation paths so VMs created
/// through either API in one process never collide.
pub fn allocate_guest_cid() -> Result<u32> {
    let used = used_guest_cids();
    let mut guard = used.lock().unwrap();
    loop {
        let cid = NEXT_GUEST_CID.load(Ordering::Relaxed);
        if cid < FIRST_GUEST_CID {
            // Counter wrapped into reserved range (<3): treat as exhausted.
            return Err(VsockConfigError::GuestCidExhausted);
        }
        // Claim `cid`; on wrap, park the counter on the 0 sentinel so later
        // callers observe exhaustion instead of re-issuing low CIDs.
        let next = cid.checked_add(1).unwrap_or(0);
        if NEXT_GUEST_CID
            .compare_exchange_weak(cid, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            continue;
        }
        if guard.contains(&cid) {
            // Explicitly reserved ahead of counter; skip it and continue.
            continue;
        }
        guard.insert(cid);
        return Ok(cid);
    }
}

/// Pin a specific guest CID for a VM.
///
/// Rejects CIDs 0, 1 and 2 ([`VsockConfigError::InvalidGuestCid`]) and CIDs
/// already allocated or reserved ([`VsockConfigError::GuestCidInUse`]); the
/// allocator will never hand out a successfully reserved CID.
pub fn reserve_guest_cid(cid: u32) -> Result<()> {
    if cid < FIRST_GUEST_CID {
        return Err(VsockConfigError::InvalidGuestCid(cid));
    }
    let used = used_guest_cids();
    let mut guard = used.lock().unwrap();
    if !guard.insert(cid) {
        return Err(VsockConfigError::GuestCidInUse(cid));
    }
    Ok(())
}

/// This struct represents the strongly typed equivalent of the json body
/// from vsock related requests.
#[derive(Clone)]
pub struct VsockDeviceConfig {
    /// ID of the vsock device.
    pub vsock_id: String,
    /// A 32-bit Context Identifier (CID) used to identify the guest.
    pub guest_cid: u32,
    /// An optional map of host to guest port mappings.
    pub host_port_map: Option<HashMap<u16, u16>>,
    /// An optional map of guest port to host UNIX domain sockets for IPC.
    pub unix_ipc_port_map: Option<HashMap<u32, (PathBuf, bool)>>,
    /// Optional custom in-process services keyed by host vsock port.
    pub custom_port_map: Option<HashMap<u32, Arc<dyn VsockPortBackend>>>,
    /// Optional custom message-oriented services keyed by host vsock port.
    #[cfg(not(target_os = "windows"))]
    pub custom_dgram_port_map: Option<HashMap<u32, Arc<dyn VsockDatagramPortBackend>>>,
    /// TSI feature flags
    pub tsi_flags: TsiFlags,
}

impl fmt::Debug for VsockDeviceConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("VsockDeviceConfig");
        debug
            .field("vsock_id", &self.vsock_id)
            .field("guest_cid", &self.guest_cid)
            .field("host_port_map", &self.host_port_map)
            .field("unix_ipc_port_map", &self.unix_ipc_port_map);
        debug.field(
            "custom_ports",
            &self
                .custom_port_map
                .as_ref()
                .map(|services| services.keys().collect::<Vec<_>>()),
        );
        #[cfg(not(target_os = "windows"))]
        debug.field(
            "custom_dgram_ports",
            &self
                .custom_dgram_port_map
                .as_ref()
                .map(|services| services.keys().collect::<Vec<_>>()),
        );
        debug.field("tsi_flags", &self.tsi_flags).finish()
    }
}

struct VsockWrapper {
    vsock: MutexVsock,
}

/// A builder of Vsock from 'VsockDeviceConfig'.
#[derive(Default)]
pub struct VsockBuilder {
    inner: Option<VsockWrapper>,
    tsi_flags: TsiFlags,
}

impl VsockBuilder {
    /// Creates an empty Vsock.
    pub fn new() -> Self {
        Self {
            inner: None,
            tsi_flags: TsiFlags::empty(),
        }
    }

    /// Inserts a Vsock in the store.
    /// If an entry already exists, it will overwrite it.
    pub fn insert(&mut self, cfg: VsockDeviceConfig) -> Result<()> {
        self.tsi_flags = cfg.tsi_flags;
        self.inner = Some(VsockWrapper {
            vsock: Arc::new(Mutex::new(Self::create_vsock(cfg)?)),
        });
        Ok(())
    }

    /// Provides a reference to the Vsock if present.
    pub fn get(&self) -> Option<&MutexVsock> {
        self.inner.as_ref().map(|pair| &pair.vsock)
    }

    pub fn tsi_flags(&self) -> TsiFlags {
        self.tsi_flags
    }

    /// Creates a Vsock device from a VsockDeviceConfig.
    pub fn create_vsock(cfg: VsockDeviceConfig) -> Result<Vsock> {
        #[cfg(not(target_os = "windows"))]
        if let Some(routes) = &cfg.custom_dgram_port_map {
            if routes.contains_key(&VSOCK_TIMESYNC_PORT) {
                return Err(VsockConfigError::ReservedDatagramPort {
                    port: VSOCK_TIMESYNC_PORT,
                    owner: "guest time synchronization",
                });
            }
            if !cfg.tsi_flags.is_empty() {
                if let Some(port) = routes
                    .keys()
                    .find(|port| (TSI_CONTROL_PORT_START..=TSI_CONTROL_PORT_END).contains(port))
                {
                    return Err(VsockConfigError::ReservedDatagramPort {
                        port: *port,
                        owner: "the active TSI control transport",
                    });
                }
            }
        }

        #[cfg(not(target_os = "windows"))]
        let custom_dgram_port_map = cfg.custom_dgram_port_map;
        #[cfg(target_os = "windows")]
        let custom_dgram_port_map = None;

        Vsock::new(
            u64::from(cfg.guest_cid),
            cfg.host_port_map,
            cfg.unix_ipc_port_map,
            cfg.custom_port_map,
            custom_dgram_port_map,
            cfg.tsi_flags,
        )
        .map_err(VsockConfigError::CreateVsockDevice)
    }
}

#[cfg(all(test, not(target_os = "windows")))]
pub(crate) mod tests {
    use std::io;

    use devices::virtio::vsock::{
        VsockDatagramBackend, VsockDatagramPeer, VsockDatagramPortBackend, VsockNotifier,
    };

    use super::*;
    use utils::tempfile::TempFile;

    struct RejectDatagrams;

    impl VsockDatagramPortBackend for RejectDatagrams {
        fn open_peer(
            &self,
            _peer: VsockDatagramPeer,
            _notifier: VsockNotifier,
        ) -> io::Result<Box<dyn VsockDatagramBackend>> {
            Err(io::Error::from(io::ErrorKind::ConnectionRefused))
        }
    }

    // Placeholder for the path where a socket file will be created.
    // The socket file will be removed when the scope ends.
    pub(crate) struct TempSockFile {
        path: String,
    }

    impl TempSockFile {
        pub fn new(tmp_file: TempFile) -> Self {
            TempSockFile {
                path: String::from(tmp_file.as_path().to_str().unwrap()),
            }
        }
    }

    impl Drop for TempSockFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub(crate) fn default_config(_tmp_sock_file: &TempSockFile) -> VsockDeviceConfig {
        let vsock_dev_id = "vsock";
        VsockDeviceConfig {
            vsock_id: vsock_dev_id.to_string(),
            guest_cid: 3,
            host_port_map: None,
            unix_ipc_port_map: None,
            custom_port_map: None,
            custom_dgram_port_map: None,
            tsi_flags: TsiFlags::empty(),
        }
    }

    #[test]
    fn test_vsock_insert() {
        let mut store = VsockBuilder::new();
        let tmp_sock_file = TempSockFile::new(TempFile::new().unwrap());
        let mut vsock_config = default_config(&tmp_sock_file);

        store.insert(vsock_config.clone()).unwrap();
        let vsock = store.get().unwrap();
        assert_eq!(vsock.lock().unwrap().id(), &vsock_config.vsock_id);

        let new_cid = vsock_config.guest_cid + 1;
        vsock_config.guest_cid = new_cid;
        store.insert(vsock_config).unwrap();
        let vsock = store.get().unwrap();
        assert_eq!(vsock.lock().unwrap().cid(), new_cid as u64);
    }

    #[test]
    fn test_guest_cid_allocation_unique_valid_and_respects_reservations() {
        use super::VsockConfigError::*;
        use std::collections::HashSet;

        // The counter is process-global, so assert properties
        // (uniqueness/validity), never exact values.
        for cid in [0, 1, 2] {
            assert!(
                matches!(reserve_guest_cid(cid), Err(InvalidGuestCid(_))),
                "CID {cid} must be rejected as reserved"
            );
        }

        let mut seen = HashSet::new();
        for _ in 0..64 {
            let cid = allocate_guest_cid().expect("CID space must not exhaust in tests");
            assert!(
                cid >= FIRST_GUEST_CID,
                "allocated CID {cid} must not be reserved"
            );
            assert!(seen.insert(cid), "allocated CID {cid} handed out twice");
        }

        // An already-allocated CID cannot be reserved.
        let allocated = allocate_guest_cid().unwrap();
        assert!(matches!(
            reserve_guest_cid(allocated),
            Err(GuestCidInUse(_))
        ));

        // Pin a CID well ahead of the monotonic counter, then drain the
        // counter past it: the allocator must skip the reservation every time.
        let pinned = allocated.wrapping_add(4096);
        assert!(pinned >= FIRST_GUEST_CID);
        reserve_guest_cid(pinned).expect("fresh CID must reserve cleanly");
        assert!(matches!(reserve_guest_cid(pinned), Err(GuestCidInUse(_))));
        loop {
            let cid = allocate_guest_cid().unwrap();
            assert_ne!(cid, pinned, "allocator handed out a reserved CID");
            if cid > pinned {
                break;
            }
        }
    }

    #[test]
    fn test_error_messages() {
        use super::VsockConfigError::*;
        use std::io;

        let err = CreateVsockDevice(devices::virtio::VsockError::EventFd(
            io::Error::from_raw_os_error(0),
        ));
        let _ = format!("{err}{err:?}");
        for err in [InvalidGuestCid(2), GuestCidInUse(3), GuestCidExhausted] {
            let _ = format!("{err}{err:?}");
        }
    }

    #[test]
    fn create_vsock_rejects_reserved_datagram_ports() {
        let tmp_sock_file = TempSockFile::new(TempFile::new().unwrap());
        let mut config = default_config(&tmp_sock_file);
        config.custom_dgram_port_map = Some(HashMap::from([(
            VSOCK_TIMESYNC_PORT,
            Arc::new(RejectDatagrams) as Arc<dyn VsockDatagramPortBackend>,
        )]));

        assert!(matches!(
            VsockBuilder::create_vsock(config),
            Err(VsockConfigError::ReservedDatagramPort {
                port: VSOCK_TIMESYNC_PORT,
                ..
            })
        ));
    }
}
