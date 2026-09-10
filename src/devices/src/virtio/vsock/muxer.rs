use std::collections::HashMap;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;

#[cfg(target_os = "macos")]
use std::sync::Condvar;

use super::super::Queue as VirtQueue;
use super::custom_stream::CustomStreamProxy;
use super::defs;
use super::defs::uapi;
use super::dgram::DatagramProxy;
use super::muxer_rxq::{rx_to_pkt, MuxerRxQ};
use super::muxer_thread::{MuxerThread, STOP_EVENT};
use super::packet::{TsiGetnameRsp, VsockPacket};
use super::proxy::{Proxy, ProxyRemoval, ProxyUpdate};
use super::reaper::ReaperThread;
#[cfg(target_os = "macos")]
use super::timesync::TimesyncThread;
use super::tsi_dgram::TsiDgramProxy;
use super::tsi_stream::TsiStreamProxy;
use super::unix::{BoundUnixListener, UnixAcceptorProxy, UnixProxy};
use super::VsockError;
use super::{
    TsiFlags, VsockConnectRequest, VsockDatagramPeer, VsockDatagramPortBackend, VsockNotifier,
    VsockPortBackend,
};
use crossbeam_channel::{unbounded, Sender};
use utils::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};
use utils::eventfd::{EventFd, EFD_NONBLOCK};
use vm_memory::GuestMemoryMmap;

use crate::virtio::InterruptTransport;
use std::net::{Ipv4Addr, SocketAddrV4};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Bound connectionless peer state per exposed host port. The least recently
/// used peer is retired before a new peer is opened at the limit.
const MAX_DGRAM_PEERS_PER_PORT: usize = 256;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

pub type ProxyMap = Arc<RwLock<HashMap<u64, Mutex<Box<dyn Proxy>>>>>;

#[derive(Clone, Copy)]
struct DgramPeerEntry {
    id: u64,
    last_used: u64,
}

/// A muxer RX queue item.
#[derive(Debug)]
pub enum MuxerRx {
    Reset {
        local_port: u32,
        peer_port: u32,
    },
    GetnameResponse {
        local_port: u32,
        peer_port: u32,
        data: TsiGetnameRsp,
    },
    ConnResponse {
        local_port: u32,
        peer_port: u32,
        result: i32,
    },
    OpRequest {
        local_port: u32,
        peer_port: u32,
    },
    OpResponse {
        local_port: u32,
        peer_port: u32,
    },
    CreditRequest {
        local_port: u32,
        peer_port: u32,
        fwd_cnt: u32,
    },
    CreditUpdate {
        local_port: u32,
        peer_port: u32,
        fwd_cnt: u32,
    },
    ListenResponse {
        local_port: u32,
        peer_port: u32,
        result: i32,
    },
    AcceptResponse {
        local_port: u32,
        peer_port: u32,
        result: i32,
    },
    Datagram {
        local_port: u32,
        peer_port: u32,
        data: Vec<u8>,
    },
}

pub fn push_packet(
    cid: u64,
    rx: MuxerRx,
    rxq_mutex: &Arc<Mutex<MuxerRxQ>>,
    queue_mutex: &Arc<Mutex<VirtQueue>>,
    mem: &GuestMemoryMmap,
) {
    let mut queue = queue_mutex.lock().unwrap();
    let mut rxq = rxq_mutex.lock().unwrap();
    if !rxq.is_empty() {
        rxq.push(rx);
        return;
    }

    if let Some(head) = queue.pop(mem) {
        if let Ok(mut pkt) = VsockPacket::from_rx_virtq_head(&head) {
            if rx_to_pkt(cid, rx, &mut pkt) {
                if let Err(e) = queue.add_used(mem, head.index, pkt.hdr().len() as u32 + pkt.len())
                {
                    error!("failed to add used elements to the queue: {e:?}");
                }
            } else {
                queue.undo_pop();
            }
        }
    } else {
        error!("couldn't push pkt to queue, adding it to rxq");
        rxq.push(rx);
    }
}

pub struct VsockMuxer {
    cid: u64,
    host_port_map: Option<HashMap<u16, u16>>,
    queue: Option<Arc<Mutex<VirtQueue>>>,
    mem: Option<GuestMemoryMmap>,
    rxq: Arc<Mutex<MuxerRxQ>>,
    epoll: Epoll,
    interrupt: Option<InterruptTransport>,
    proxy_map: ProxyMap,
    reaper_sender: Option<Sender<u64>>,
    unix_ipc_port_map: Option<HashMap<u32, (PathBuf, bool)>>,
    host_listeners: HashMap<u32, Arc<BoundUnixListener>>,
    custom_port_map: Option<HashMap<u32, Arc<dyn VsockPortBackend>>>,
    custom_dgram_port_map: Option<HashMap<u32, Arc<dyn VsockDatagramPortBackend>>>,
    dgram_peer_map: Mutex<HashMap<(u32, u32), DgramPeerEntry>>,
    next_dgram_proxy_id: AtomicU64,
    next_dgram_activity: AtomicU64,
    tsi_flags: TsiFlags,
    stop_evt: EventFd,
    muxer_thread: Option<JoinHandle<()>>,
    reaper_thread: Option<JoinHandle<()>>,
    retired: bool,
    #[cfg(target_os = "macos")]
    timesync_stop: Arc<(Mutex<bool>, Condvar)>,
    #[cfg(target_os = "macos")]
    timesync_thread: Option<JoinHandle<()>>,
}

impl VsockMuxer {
    pub(crate) fn new(
        cid: u64,
        host_port_map: Option<HashMap<u16, u16>>,
        unix_ipc_port_map: Option<HashMap<u32, (PathBuf, bool)>>,
        custom_port_map: Option<HashMap<u32, Arc<dyn VsockPortBackend>>>,
        custom_dgram_port_map: Option<HashMap<u32, Arc<dyn VsockDatagramPortBackend>>>,
        tsi_flags: TsiFlags,
    ) -> super::Result<Self> {
        let mut host_listeners = HashMap::new();
        for (port, (path, listen)) in unix_ipc_port_map.iter().flatten() {
            if *listen {
                let listener =
                    BoundUnixListener::bind(path).map_err(|error| VsockError::HostListener {
                        port: *port,
                        path: path.clone(),
                        error,
                    })?;
                host_listeners.insert(*port, Arc::new(listener));
            }
        }
        Ok(VsockMuxer {
            cid,
            host_port_map,
            queue: None,
            mem: None,
            rxq: Arc::new(Mutex::new(MuxerRxQ::new())),
            epoll: Epoll::new().map_err(VsockError::Transport)?,
            interrupt: None,
            proxy_map: Arc::new(RwLock::new(HashMap::new())),
            reaper_sender: None,
            unix_ipc_port_map,
            host_listeners,
            custom_port_map,
            custom_dgram_port_map,
            dgram_peer_map: Mutex::new(HashMap::new()),
            // Direct stream proxy ids use the low 32 bits for a non-zero host
            // port. Keeping those bits zero gives datagram event tokens a
            // disjoint namespace without changing the legacy stream ids.
            next_dgram_proxy_id: AtomicU64::new(1),
            next_dgram_activity: AtomicU64::new(1),
            tsi_flags,
            stop_evt: EventFd::new(EFD_NONBLOCK).map_err(VsockError::EventFd)?,
            muxer_thread: None,
            reaper_thread: None,
            retired: false,
            #[cfg(target_os = "macos")]
            timesync_stop: Arc::new((Mutex::new(false), Condvar::new())),
            #[cfg(target_os = "macos")]
            timesync_thread: None,
        })
    }

    pub(crate) fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        queue: Arc<Mutex<VirtQueue>>,
        interrupt: InterruptTransport,
    ) -> std::io::Result<()> {
        if self.retired {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "vsock device has exited",
            ));
        }
        if self.muxer_thread.is_some() || self.reaper_thread.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "vsock muxer is already activated",
            ));
        }
        let result = self.activate_inner(mem, queue, interrupt);
        if result.is_err() {
            // Partial activation must not leave a worker or poll registration behind.
            self.quiesce()?;
        }
        result
    }

    fn activate_inner(
        &mut self,
        mem: GuestMemoryMmap,
        queue: Arc<Mutex<VirtQueue>>,
        interrupt: InterruptTransport,
    ) -> std::io::Result<()> {
        let stop_evt = self.stop_evt.try_clone()?;
        self.epoll.ctl(
            ControlOperation::Add,
            self.stop_evt.as_raw_fd(),
            &EpollEvent::new(EventSet::IN, STOP_EVENT),
        )?;
        for (port, listener) in &self.host_listeners {
            listener.check_path()?;
            let id = ((*port as u64) << 32) | (defs::TSI_PROXY_PORT as u64);
            let proxy = UnixAcceptorProxy::new(id, listener.clone(), *port);
            self.epoll.ctl(
                ControlOperation::Add,
                proxy.as_raw_fd(),
                &EpollEvent::new(EventSet::IN, id),
            )?;
            self.proxy_map
                .write()
                .unwrap()
                .insert(id, Mutex::new(Box::new(proxy)));
        }
        self.queue = Some(queue.clone());
        self.mem = Some(mem.clone());
        self.interrupt = Some(interrupt.clone());

        #[cfg(target_os = "macos")]
        {
            *self.timesync_stop.0.lock().unwrap() = false;
            let timesync = TimesyncThread::new(
                self.cid,
                mem.clone(),
                queue.clone(),
                interrupt.clone(),
                Arc::clone(&self.timesync_stop),
            );
            self.timesync_thread = Some(timesync.run()?);
        }

        let (sender, receiver) = unbounded();
        self.reaper_sender = Some(sender.clone());
        self.reaper_thread = Some(ReaperThread::new(receiver, self.proxy_map.clone()).try_run()?);

        let thread = MuxerThread::new(
            self.cid,
            self.epoll.clone(),
            self.rxq.clone(),
            self.proxy_map.clone(),
            mem,
            queue,
            interrupt.clone(),
            sender,
            stop_evt,
        );
        self.muxer_thread = Some(thread.run()?);
        Ok(())
    }

    /// Stops every background writer and drops connection-local proxy state.
    pub(crate) fn quiesce(&mut self) -> std::io::Result<()> {
        let mut result = Ok(());
        if self.muxer_thread.is_some() {
            self.stop_evt.write(1)?;
            result = self
                .muxer_thread
                .take()
                .unwrap()
                .join()
                .map_err(|_| std::io::Error::other("vsock muxer thread panicked"));
        }

        #[cfg(target_os = "macos")]
        if let Some(thread) = self.timesync_thread.take() {
            let (stopped, changed) = &*self.timesync_stop;
            *stopped.lock().unwrap() = true;
            changed.notify_all();
            let joined = thread
                .join()
                .map_err(|_| std::io::Error::other("vsock timesync thread panicked"));
            result = result.and(joined);
        }

        // The muxer owned the only other sender. Dropping this endpoint lets the reaper terminate
        // without a second stop protocol.
        self.reaper_sender.take();
        if let Some(thread) = self.reaper_thread.take() {
            let joined = thread
                .join()
                .map_err(|_| std::io::Error::other("vsock reaper thread panicked"));
            result = result.and(joined);
        }

        // Listener descriptors survive quiescence; their registrations must not.
        for listener in self.host_listeners.values() {
            let _ = self.epoll.ctl(
                ControlOperation::Delete,
                listener.as_raw_fd(),
                &EpollEvent::default(),
            );
        }
        let _ = self.epoll.ctl(
            ControlOperation::Delete,
            self.stop_evt.as_raw_fd(),
            &EpollEvent::default(),
        );
        let _ = self.stop_evt.read();
        self.proxy_map.write().unwrap().clear();
        self.dgram_peer_map.lock().unwrap().clear();
        self.rxq.lock().unwrap().clear();
        self.queue = None;
        self.mem = None;
        self.interrupt = None;
        result
    }

    /// Release transport endpoints before the VMM's process-level exit.
    pub(crate) fn retire(&mut self) -> std::io::Result<()> {
        self.retired = true;
        let result = self.quiesce();
        self.host_listeners.clear();
        result
    }

    pub(crate) fn has_pending_rx(&self) -> bool {
        !self.rxq.lock().unwrap().is_empty()
    }

    pub(crate) fn recv_pkt(&mut self, pkt: &mut VsockPacket) -> super::Result<()> {
        debug!("recv_stream_pkt");
        if self.rxq.lock().unwrap().is_empty() {
            return Err(VsockError::NoData);
        }

        let mut rxq = self.rxq.lock().unwrap();
        while let Some(rx) = rxq.pop() {
            if rx_to_pkt(self.cid, rx, pkt) {
                return Ok(());
            }
        }
        Err(VsockError::NoData)
    }

    /// Retry proxy work after the caller has released the guest RX queue.
    pub(crate) fn kick_backends(&self) {
        for proxy in self.proxy_map.read().unwrap().values() {
            proxy.lock().unwrap().kick();
        }
    }

    fn push_packet(&self, rx: MuxerRx) {
        let mem = match self.mem.as_ref() {
            Some(m) => m,
            None => {
                error!("proxy creation without mem");
                return;
            }
        };
        let queue_mutex = match self.queue.as_ref() {
            Some(q) => q,
            None => {
                error!("stream proxy creation without stream queue");
                return;
            }
        };

        let mut queue = queue_mutex.lock().unwrap();
        let mut rxq = self.rxq.lock().unwrap();
        if !rxq.is_empty() {
            rxq.push(rx);
            return;
        }

        if let Some(head) = queue.pop(mem) {
            if let Ok(mut pkt) = VsockPacket::from_rx_virtq_head(&head) {
                if rx_to_pkt(self.cid, rx, &mut pkt) {
                    if let Err(e) =
                        queue.add_used(mem, head.index, pkt.hdr().len() as u32 + pkt.len())
                    {
                        error!("failed to add used elements to the queue: {e:?}");
                    }
                } else {
                    queue.undo_pop();
                }
            }
        } else {
            error!("couldn't push pkt to queue, adding it to rxq");
            rxq.push(rx);
        }
    }

    pub fn update_polling(&self, id: u64, fd: RawFd, evset: EventSet) {
        debug!("update_polling id={id} fd={fd:?} evset={evset:?}");
        let _ = self
            .epoll
            .ctl(ControlOperation::Delete, fd, &EpollEvent::default());
        if !evset.is_empty() {
            let _ = self
                .epoll
                .ctl(ControlOperation::Add, fd, &EpollEvent::new(evset, id));
        }
    }

    fn process_proxy_update(&self, id: u64, update: ProxyUpdate) {
        if let Some(polling) = update.polling {
            self.update_polling(polling.0, polling.1, polling.2);
        }

        match update.remove_proxy {
            ProxyRemoval::Keep => {}
            ProxyRemoval::Immediate => {
                info!("immediately removing proxy: {id}");
                self.remove_proxy(id);
            }
            ProxyRemoval::Deferred => {
                info!("deferring proxy removal: {id}");
                if let Some(reaper_sender) = &self.reaper_sender {
                    if reaper_sender.send(id).is_err() {
                        self.proxy_map.write().unwrap().remove(&id);
                    }
                }
            }
        }

        if update.signal_queue {
            if let Some(interrupt) = &self.interrupt {
                interrupt.signal_used_queue();
            }
        }
    }

    /// Remove polling, proxy ownership, and any connectionless peer index as
    /// one lifecycle operation.
    fn remove_proxy(&self, id: u64) {
        let proxy = self.proxy_map.write().unwrap().remove(&id);
        if let Some(proxy) = proxy {
            let pollable = proxy.lock().unwrap().pollable();
            self.update_polling(id, pollable, EventSet::empty());
        }
        self.dgram_peer_map
            .lock()
            .unwrap()
            .retain(|_, entry| entry.id != id);
    }

    fn evict_oldest_dgram_peer(&self, host_port: u32) {
        let evicted = {
            let mut peers = self.dgram_peer_map.lock().unwrap();
            if peers.keys().filter(|(_, port)| *port == host_port).count()
                < MAX_DGRAM_PEERS_PER_PORT
            {
                None
            } else {
                let oldest = peers
                    .iter()
                    .filter(|((_, port), _)| *port == host_port)
                    .min_by_key(|(_, entry)| entry.last_used)
                    .map(|(key, entry)| (*key, entry.id));
                oldest.and_then(|(key, id)| peers.remove(&key).map(|_| id))
            }
        };

        if let Some(id) = evicted {
            self.remove_proxy(id);
        }
    }

    fn process_proxy_create(&self, pkt: &VsockPacket) {
        debug!("proxy create request");
        if let Some(req) = pkt.read_proxy_create() {
            if self.host_listeners.contains_key(&req.peer_port) {
                // Host listeners are device-owned, not guest-created TSI sockets.
                return;
            }
            debug!(
                "proxy create request: peer_port={}, type={}",
                req.peer_port, req._type
            );
            let mem = match self.mem.as_ref() {
                Some(m) => m,
                None => {
                    error!("proxy creation without mem");
                    return;
                }
            };
            let queue = match self.queue.as_ref() {
                Some(q) => q,
                None => {
                    error!("stream proxy creation without stream queue");
                    return;
                }
            };
            match req._type {
                defs::SOCK_STREAM => {
                    debug!("proxy create stream");
                    let id = ((req.peer_port as u64) << 32) | (defs::TSI_PROXY_PORT as u64);
                    if req.family as i32 == libc::AF_UNIX
                        && !self.tsi_flags.contains(TsiFlags::HIJACK_UNIX)
                    {
                        warn!("rejecting stream unix proxy because HIJACK_UNIX is disabled");
                        return;
                    }
                    if (req.family as i32 == libc::AF_INET || req.family as i32 == libc::AF_INET6)
                        && !self.tsi_flags.contains(TsiFlags::HIJACK_INET)
                    {
                        warn!("rejecting stream inet proxy because HIJACK_INET is disabled");
                        return;
                    }
                    match TsiStreamProxy::new(
                        id,
                        self.cid,
                        req.family,
                        defs::TSI_PROXY_PORT,
                        req.peer_port,
                        pkt.src_port(),
                        mem.clone(),
                        queue.clone(),
                        self.rxq.clone(),
                    ) {
                        Ok(proxy) => {
                            self.proxy_map
                                .write()
                                .unwrap()
                                .insert(id, Mutex::new(Box::new(proxy)));
                        }
                        Err(e) => debug!("error creating tcp proxy: {e}"),
                    }
                }
                defs::SOCK_DGRAM => {
                    debug!("proxy create dgram");
                    let id = ((req.peer_port as u64) << 32) | (defs::TSI_PROXY_PORT as u64);
                    if req.family as i32 == libc::AF_UNIX
                        && !self.tsi_flags.contains(TsiFlags::HIJACK_UNIX)
                    {
                        warn!("rejecting dgram unix proxy because HIJACK_UNIX is disabled");
                        return;
                    }
                    if (req.family as i32 == libc::AF_INET || req.family as i32 == libc::AF_INET6)
                        && !self.tsi_flags.contains(TsiFlags::HIJACK_INET)
                    {
                        warn!("rejecting dgram inet proxy because HIJACK_INET is disabled");
                        return;
                    }
                    match TsiDgramProxy::new(
                        id,
                        self.cid,
                        req.family,
                        req.peer_port,
                        mem.clone(),
                        queue.clone(),
                        self.rxq.clone(),
                    ) {
                        Ok(proxy) => {
                            self.proxy_map
                                .write()
                                .unwrap()
                                .insert(id, Mutex::new(Box::new(proxy)));
                        }
                        Err(e) => debug!("error creating udp proxy: {e}"),
                    }
                }
                _ => debug!("unknown type on connection request"),
            };
        }
    }

    fn process_connect(&self, pkt: &VsockPacket) {
        debug!("proxy connect request");
        if let Some(req) = pkt.read_connect_req() {
            let id = ((req.peer_port as u64) << 32) | (defs::TSI_PROXY_PORT as u64);
            debug!("proxy connect request: id={id}");
            match self.proxy_map.read().unwrap().get(&id) {
                Some(proxy) => {
                    self.process_proxy_update(id, proxy.lock().unwrap().connect(pkt, req));
                }
                None => self.push_packet(MuxerRx::ConnResponse {
                    local_port: pkt.dst_port(),
                    peer_port: pkt.src_port(),
                    result: -libc::ECONNREFUSED,
                }),
            }
        }
    }

    fn process_getname(&self, pkt: &VsockPacket) {
        debug!("new getname request");
        if let Some(req) = pkt.read_getname_req() {
            let id = ((req.peer_port as u64) << 32) | (req.local_port as u64);
            debug!(
                "new getname request: id={}, peer_port={}, local_port={}",
                id, req.peer_port, req.local_port
            );

            match self.proxy_map.read().unwrap().get(&id) {
                Some(proxy) => proxy.lock().unwrap().getpeername(pkt),
                None => self.push_packet(MuxerRx::GetnameResponse {
                    local_port: pkt.dst_port(),
                    peer_port: pkt.src_port(),
                    data: TsiGetnameRsp {
                        result: -libc::EINVAL,
                        addr_len: 0,
                        addr: SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0).into(),
                    },
                }),
            }
        }
    }

    fn process_sendto_addr(&self, pkt: &VsockPacket) {
        debug!("new DGRAM sendto addr: src={}", pkt.src_port());
        if let Some(req) = pkt.read_sendto_addr() {
            let id = ((req.peer_port as u64) << 32) | (defs::TSI_PROXY_PORT as u64);
            debug!("new DGRAM sendto addr: id={id}");
            let update = self
                .proxy_map
                .read()
                .unwrap()
                .get(&id)
                .map(|proxy| proxy.lock().unwrap().sendto_addr(req));

            if let Some(update) = update {
                self.process_proxy_update(id, update);
            }
        }
    }

    fn process_sendto_data(&self, pkt: &VsockPacket) {
        let id = ((pkt.src_port() as u64) << 32) | (defs::TSI_PROXY_PORT as u64);
        debug!("DGRAM sendto data: id={} src={}", id, pkt.src_port());
        if let Some(proxy) = self.proxy_map.read().unwrap().get(&id) {
            proxy.lock().unwrap().sendto_data(pkt);
        }
    }

    fn process_listen_request(&self, pkt: &VsockPacket) {
        debug!("DGRAM listen request: src={}", pkt.src_port());
        if let Some(req) = pkt.read_listen_req() {
            let id = ((req.peer_port as u64) << 32) | (defs::TSI_PROXY_PORT as u64);
            debug!("DGRAM listen request: id={id}");
            match self.proxy_map.read().unwrap().get(&id) {
                Some(proxy) => self.process_proxy_update(
                    id,
                    proxy.lock().unwrap().listen(pkt, req, &self.host_port_map),
                ),
                None => self.push_packet(MuxerRx::ListenResponse {
                    local_port: pkt.dst_port(),
                    peer_port: pkt.src_port(),
                    result: -libc::EPERM,
                }),
            };
        }
    }

    fn process_accept_request(&self, pkt: &VsockPacket) {
        debug!("DGRAM accept request: src={}", pkt.src_port());
        if let Some(req) = pkt.read_accept_req() {
            let id = ((req.peer_port as u64) << 32) | (defs::TSI_PROXY_PORT as u64);
            debug!("DGRAM accept request: id={id}");
            match self.proxy_map.read().unwrap().get(&id) {
                Some(proxy) => self.process_proxy_update(id, proxy.lock().unwrap().accept(req)),
                None => self.push_packet(MuxerRx::AcceptResponse {
                    local_port: pkt.dst_port(),
                    peer_port: pkt.src_port(),
                    result: -libc::EINVAL,
                }),
            }
        }
    }

    fn process_proxy_release(&self, pkt: &VsockPacket) {
        debug!("DGRAM release request: src={}", pkt.src_port());
        if let Some(req) = pkt.read_release_req() {
            let id = ((req.peer_port as u64) << 32) | (req.local_port as u64);
            debug!(
                "DGRAM release request: id={} local_port={} peer_port={}",
                id, req.local_port, req.peer_port
            );
            let update = if let Some(proxy) = self.proxy_map.read().unwrap().get(&id) {
                Some(proxy.lock().unwrap().release())
            } else {
                debug!(
                    "release without proxy: id={}, proxies={}",
                    id,
                    self.proxy_map.read().unwrap().len()
                );
                None
            };

            if let Some(update) = update {
                self.process_proxy_update(id, update);
            }
        }
        debug!(
            "DGRAM release request: proxies={}",
            self.proxy_map.read().unwrap().len()
        );
    }

    fn process_dgram_rw(&self, pkt: &VsockPacket) {
        debug!("DGRAM OP_RW");
        let id = ((pkt.src_port() as u64) << 32) | (defs::TSI_PROXY_PORT as u64);

        let update = self
            .proxy_map
            .read()
            .unwrap()
            .get(&id)
            .map(|proxy| proxy.lock().unwrap().sendmsg(pkt));
        if let Some(update) = update {
            debug!("DGRAM allowing OP_RW for {}", pkt.src_port());
            self.process_proxy_update(id, update);
        } else {
            debug!("DGRAM ignoring OP_RW for {}", pkt.src_port());
        }
    }

    fn process_custom_dgram(&self, pkt: &VsockPacket) {
        let Some(service) = self
            .custom_dgram_port_map
            .as_ref()
            .and_then(|routes| routes.get(&pkt.dst_port()))
            .cloned()
        else {
            return;
        };

        let peer_key = (pkt.src_port(), pkt.dst_port());
        let activity = self.next_dgram_activity.fetch_add(1, Ordering::Relaxed);
        let existing = {
            let mut peers = self.dgram_peer_map.lock().unwrap();
            peers.get_mut(&peer_key).map(|entry| {
                entry.last_used = activity;
                entry.id
            })
        };
        if let Some(id) = existing {
            let update = self
                .proxy_map
                .read()
                .unwrap()
                .get(&id)
                .map(|proxy| proxy.lock().unwrap().sendmsg(pkt));
            if let Some(update) = update {
                self.process_proxy_update(id, update);
                return;
            }
            self.dgram_peer_map.lock().unwrap().remove(&peer_key);
        }

        self.evict_oldest_dgram_peer(pkt.dst_port());

        let Some(mem) = self.mem.as_ref() else {
            warn!("vsock datagram without guest memory");
            return;
        };
        let Some(queue) = self.queue.as_ref() else {
            warn!("vsock datagram without receive queue");
            return;
        };

        let notifier = match VsockNotifier::new() {
            Ok(notifier) => notifier,
            Err(err) => {
                warn!("failed to create custom vsock datagram notifier: {err}");
                return;
            }
        };
        let peer = VsockDatagramPeer {
            guest_cid: self.cid,
            guest_port: pkt.src_port(),
            host_port: pkt.dst_port(),
        };
        let backend = match service.open_peer(peer, notifier.clone()) {
            Ok(backend) => backend,
            Err(err) => {
                warn!(
                    "custom vsock datagram service rejected port {}: {err}",
                    pkt.dst_port()
                );
                return;
            }
        };

        let id = self.next_dgram_proxy_id.fetch_add(1, Ordering::Relaxed) << 32;
        let proxy = DatagramProxy::new(
            id,
            self.cid,
            pkt.dst_port(),
            pkt.src_port(),
            backend,
            notifier,
            mem.clone(),
            queue.clone(),
            self.rxq.clone(),
        );
        let poll_fd = proxy.as_raw_fd();
        if poll_fd < 0 {
            warn!(
                "custom vsock datagram service for port {} returned an invalid poll fd",
                pkt.dst_port()
            );
            return;
        }

        // Publish the proxy before registering its pollable. A host service can
        // reply synchronously from `sendmsg`; if readiness wakes the muxer
        // thread before this map entry exists, that event can be consumed with
        // no proxy available to drain the datagram.
        self.proxy_map
            .write()
            .unwrap()
            .insert(id, Mutex::new(Box::new(proxy)));
        if let Err(err) = self.epoll.ctl(
            ControlOperation::Add,
            poll_fd,
            &EpollEvent::new(EventSet::IN, id),
        ) {
            warn!(
                "custom vsock datagram service for port {} returned an unusable poll fd: {err}",
                pkt.dst_port()
            );
            self.proxy_map.write().unwrap().remove(&id);
            return;
        }

        self.dgram_peer_map.lock().unwrap().insert(
            peer_key,
            DgramPeerEntry {
                id,
                last_used: activity,
            },
        );
        let update = self
            .proxy_map
            .read()
            .unwrap()
            .get(&id)
            .map(|proxy| proxy.lock().unwrap().sendmsg(pkt));
        if let Some(update) = update {
            self.process_proxy_update(id, update);
        }
    }

    pub(crate) fn send_dgram_pkt(&mut self, pkt: &VsockPacket) -> super::Result<()> {
        debug!(
            "send_dgram_pkt: src_port={} dst_port={}",
            pkt.src_port(),
            pkt.dst_port()
        );

        if pkt.src_cid() != self.cid || pkt.dst_cid() != uapi::VSOCK_HOST_CID {
            debug!("dropping guest packet with invalid CIDs: {:?}", pkt.hdr());
            return Ok(());
        }

        if self
            .custom_dgram_port_map
            .as_ref()
            .is_some_and(|routes| routes.contains_key(&pkt.dst_port()))
        {
            if pkt.op() == uapi::VSOCK_OP_RW {
                self.process_custom_dgram(pkt);
            } else {
                debug!("dropping non-RW packet for direct datagram route");
            }
            return Ok(());
        }

        match pkt.dst_port() {
            defs::TSI_PROXY_CREATE if self.tsi_flags.tsi_enabled() => {
                self.process_proxy_create(pkt)
            }
            defs::TSI_CONNECT if self.tsi_flags.tsi_enabled() => self.process_connect(pkt),
            defs::TSI_GETNAME if self.tsi_flags.tsi_enabled() => self.process_getname(pkt),
            defs::TSI_SENDTO_ADDR if self.tsi_flags.tsi_enabled() => self.process_sendto_addr(pkt),
            defs::TSI_SENDTO_DATA if self.tsi_flags.tsi_enabled() => self.process_sendto_data(pkt),
            defs::TSI_LISTEN if self.tsi_flags.tsi_enabled() => self.process_listen_request(pkt),
            defs::TSI_ACCEPT if self.tsi_flags.tsi_enabled() => self.process_accept_request(pkt),
            defs::TSI_PROXY_RELEASE if self.tsi_flags.tsi_enabled() => {
                self.process_proxy_release(pkt)
            }
            _ => {
                if pkt.op() == uapi::VSOCK_OP_RW {
                    self.process_dgram_rw(pkt);
                } else {
                    error!("unexpected dgram pkt: {}", pkt.op());
                }
            }
        }

        Ok(())
    }

    fn process_op_request(&mut self, pkt: &VsockPacket) {
        debug!("OP_REQUEST");
        let id: u64 = ((pkt.src_port() as u64) << 32) | (pkt.dst_port() as u64);

        let existing = self
            .proxy_map
            .read()
            .unwrap()
            .get(&id)
            .map(|proxy| proxy.lock().unwrap().confirm_connect(pkt));
        if let Some(update) = existing {
            if let Some(update) = update {
                self.process_proxy_update(id, update);
            }
            return;
        }

        let Some(mem) = self.mem.as_ref() else {
            warn!("vsock connection request without guest memory");
            return;
        };
        let Some(queue) = self.queue.as_ref() else {
            warn!("vsock connection request without receive queue");
            return;
        };

        if let Some(service) = self
            .custom_port_map
            .as_ref()
            .and_then(|routes| routes.get(&pkt.dst_port()))
            .cloned()
        {
            let request = VsockConnectRequest {
                guest_cid: self.cid,
                guest_port: pkt.src_port(),
                host_port: pkt.dst_port(),
            };
            let notifier = match VsockNotifier::new() {
                Ok(notifier) => notifier,
                Err(err) => {
                    warn!("failed to create custom vsock notifier: {err}");
                    push_packet(
                        self.cid,
                        MuxerRx::Reset {
                            local_port: pkt.dst_port(),
                            peer_port: pkt.src_port(),
                        },
                        &self.rxq,
                        queue,
                        mem,
                    );
                    return;
                }
            };
            match service.connect(request, notifier.clone()) {
                Ok(backend) => {
                    let mut proxy = match CustomStreamProxy::new(
                        id,
                        self.cid,
                        pkt.dst_port(),
                        pkt.src_port(),
                        backend,
                        notifier,
                        mem.clone(),
                        queue.clone(),
                        self.rxq.clone(),
                    ) {
                        Ok(proxy) => proxy,
                        Err(err) => {
                            warn!(
                                "custom vsock service failed to initialize port {}: {err}",
                                pkt.dst_port()
                            );
                            push_packet(
                                self.cid,
                                MuxerRx::Reset {
                                    local_port: pkt.dst_port(),
                                    peer_port: pkt.src_port(),
                                },
                                &self.rxq,
                                queue,
                                mem,
                            );
                            return;
                        }
                    };
                    let connecting = proxy.is_connecting();
                    if connecting {
                        proxy.prepare_connect(pkt);
                    }
                    let poll_fd = proxy.pollable();
                    if poll_fd < 0 {
                        warn!(
                            "custom vsock service for port {} returned an invalid poll fd",
                            pkt.dst_port()
                        );
                        push_packet(
                            self.cid,
                            MuxerRx::Reset {
                                local_port: pkt.dst_port(),
                                peer_port: pkt.src_port(),
                            },
                            &self.rxq,
                            queue,
                            mem,
                        );
                        return;
                    }
                    self.proxy_map
                        .write()
                        .unwrap()
                        .insert(id, Mutex::new(Box::new(proxy)));
                    if let Err(err) = self.epoll.ctl(
                        ControlOperation::Add,
                        poll_fd,
                        &EpollEvent::new(
                            if connecting {
                                EventSet::IN | EventSet::OUT
                            } else {
                                EventSet::IN
                            },
                            id,
                        ),
                    ) {
                        self.proxy_map.write().unwrap().remove(&id);
                        warn!(
                            "custom vsock service for port {} returned an unusable poll fd: {err}",
                            pkt.dst_port()
                        );
                        push_packet(
                            self.cid,
                            MuxerRx::Reset {
                                local_port: pkt.dst_port(),
                                peer_port: pkt.src_port(),
                            },
                            &self.rxq,
                            queue,
                            mem,
                        );
                        return;
                    }
                    if !connecting {
                        if let Some(proxy) = self.proxy_map.read().unwrap().get(&id) {
                            proxy.lock().unwrap().confirm_connect(pkt);
                        }
                    }
                }
                Err(err) => {
                    warn!(
                        "custom vsock service rejected port {}: {err}",
                        pkt.dst_port()
                    );
                    push_packet(
                        self.cid,
                        MuxerRx::Reset {
                            local_port: pkt.dst_port(),
                            peer_port: pkt.src_port(),
                        },
                        &self.rxq,
                        queue,
                        mem,
                    );
                }
            }
            return;
        }

        if let Some((path, listen)) = self
            .unix_ipc_port_map
            .as_ref()
            .and_then(|routes| routes.get(&pkt.dst_port()))
        {
            if *listen {
                warn!("attempting to connect a vsock port configured for Unix listen mode");
                push_packet(
                    self.cid,
                    MuxerRx::Reset {
                        local_port: pkt.dst_port(),
                        peer_port: pkt.src_port(),
                    },
                    &self.rxq,
                    queue,
                    mem,
                );
                return;
            }

            let mut unix = match UnixProxy::new(
                id,
                self.cid,
                pkt.dst_port(),
                pkt.src_port(),
                mem.clone(),
                queue.clone(),
                self.rxq.clone(),
                path.to_path_buf(),
            ) {
                Ok(proxy) => proxy,
                Err(err) => {
                    warn!(
                        "failed to create Unix proxy for host port {}: {err}",
                        pkt.dst_port()
                    );
                    push_packet(
                        self.cid,
                        MuxerRx::Reset {
                            local_port: pkt.dst_port(),
                            peer_port: pkt.src_port(),
                        },
                        &self.rxq,
                        queue,
                        mem,
                    );
                    return;
                }
            };
            let update = match unix.connect_vsock() {
                Ok(update) => update,
                Err(errno) => {
                    warn!(
                        "failed to connect Unix route for host port {}: errno {}",
                        pkt.dst_port(),
                        errno
                    );
                    push_packet(
                        self.cid,
                        MuxerRx::Reset {
                            local_port: pkt.dst_port(),
                            peer_port: pkt.src_port(),
                        },
                        &self.rxq,
                        queue,
                        mem,
                    );
                    return;
                }
            };
            if unix.status == super::proxy::ProxyStatus::Connected {
                unix.confirm_vsock_connect(pkt);
            } else {
                unix.prepare_vsock_connect(pkt);
            }
            self.proxy_map
                .write()
                .unwrap()
                .insert(id, Mutex::new(Box::new(unix)));
            self.process_proxy_update(id, update);
        }
    }

    fn process_op_response(&self, pkt: &VsockPacket) {
        debug!("OP_RESPONSE");
        let id: u64 = ((pkt.src_port() as u64) << 32) | (pkt.dst_port() as u64);
        let update = self
            .proxy_map
            .read()
            .unwrap()
            .get(&id)
            .map(|proxy| proxy.lock().unwrap().process_op_response(pkt));
        update
            .as_ref()
            .and_then(|u| u.push_accept)
            .and_then(|(_id, parent_id)| {
                self.proxy_map
                    .read()
                    .unwrap()
                    .get(&parent_id)
                    .map(|proxy| proxy.lock().unwrap().enqueue_accept())
            });

        if let Some(update) = update {
            self.process_proxy_update(id, update);
        }
    }

    fn process_op_shutdown(&self, pkt: &VsockPacket) {
        debug!("OP_SHUTDOWN");
        let id: u64 = ((pkt.src_port() as u64) << 32) | (pkt.dst_port() as u64);
        if let Some(proxy) = self.proxy_map.read().unwrap().get(&id) {
            proxy.lock().unwrap().shutdown(pkt);
        }
    }

    fn process_op_credit_update(&self, pkt: &VsockPacket) {
        debug!("OP_CREDIT_UPDATE");
        let id: u64 = ((pkt.src_port() as u64) << 32) | (pkt.dst_port() as u64);
        let update = self
            .proxy_map
            .read()
            .unwrap()
            .get(&id)
            .map(|proxy| proxy.lock().unwrap().update_peer_credit(pkt));
        if let Some(update) = update {
            self.process_proxy_update(id, update);
        }
    }

    fn process_stream_rw(&self, pkt: &VsockPacket) {
        debug!("OP_RW");
        let id: u64 = ((pkt.src_port() as u64) << 32) | (pkt.dst_port() as u64);
        let update = self
            .proxy_map
            .read()
            .unwrap()
            .get(&id)
            .map(|proxy| proxy.lock().unwrap().sendmsg(pkt));
        if let Some(update) = update {
            debug!(
                "allowing OP_RW: src={} dst={}",
                pkt.src_port(),
                pkt.dst_port()
            );
            self.process_proxy_update(id, update);
        } else {
            debug!("invalid OP_RW for {}, sending reset", pkt.src_port());
            let mem = match self.mem.as_ref() {
                Some(m) => m,
                None => {
                    warn!("OP_RW without mem");
                    return;
                }
            };
            let queue = match self.queue.as_ref() {
                Some(q) => q,
                None => {
                    warn!("OP_RW without queue");
                    return;
                }
            };

            // This response goes to the connection.
            let rx = MuxerRx::Reset {
                local_port: pkt.dst_port(),
                peer_port: pkt.src_port(),
            };
            push_packet(self.cid, rx, &self.rxq, queue, mem);
        }
    }

    fn process_stream_rst(&self, pkt: &VsockPacket) {
        debug!("OP_RST");
        let id: u64 = ((pkt.src_port() as u64) << 32) | (pkt.dst_port() as u64);
        let update = self
            .proxy_map
            .read()
            .unwrap()
            .get(&id)
            .map(|proxy| proxy.lock().unwrap().release());
        if let Some(update) = update {
            debug!(
                "allowing OP_RST: id={} src={} dst={}",
                id,
                pkt.src_port(),
                pkt.dst_port()
            );
            self.process_proxy_update(id, update);
        } else {
            debug!("invalid OP_RST for {}", pkt.src_port());
        }
    }

    pub(crate) fn send_stream_pkt(&mut self, pkt: &VsockPacket) -> super::Result<()> {
        debug!(
            "send_pkt: src_port={} dst_port={}, op={}",
            pkt.src_port(),
            pkt.dst_port(),
            pkt.op()
        );

        if pkt.src_cid() != self.cid || pkt.dst_cid() != uapi::VSOCK_HOST_CID {
            debug!("dropping guest packet with invalid CIDs: {:?}", pkt.hdr());
            return Ok(());
        }

        match pkt.op() {
            uapi::VSOCK_OP_REQUEST => self.process_op_request(pkt),
            uapi::VSOCK_OP_RESPONSE => self.process_op_response(pkt),
            uapi::VSOCK_OP_SHUTDOWN => self.process_op_shutdown(pkt),
            uapi::VSOCK_OP_CREDIT_UPDATE => self.process_op_credit_update(pkt),
            uapi::VSOCK_OP_RW => self.process_stream_rw(pkt),
            uapi::VSOCK_OP_RST => self.process_stream_rst(pkt),
            _ => warn!("stream: unhandled op={}", pkt.op()),
        }
        Ok(())
    }
}

impl Drop for VsockMuxer {
    fn drop(&mut self) {
        if let Err(error) = self.quiesce() {
            error!("failed to stop vsock workers during device teardown: {error}");
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::super::packet::VSOCK_PKT_HDR_SIZE;
    use super::super::{VsockDatagramBackend, VsockStreamBackend};
    use super::*;
    use crate::virtio::{Descriptor, DescriptorChain};
    use std::io;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::time::{Duration, Instant};
    use utils::tempdir::TempDir;
    use vm_memory::{Bytes, GuestAddress};

    const TEST_CID: u64 = 42;
    const TEST_PORT: u32 = 5000;
    const HEADER: GuestAddress = GuestAddress(0x2000);

    fn listener_muxer(path: &std::path::Path) -> super::super::Result<VsockMuxer> {
        VsockMuxer::new(
            TEST_CID,
            None,
            Some(HashMap::from([(TEST_PORT, (path.to_path_buf(), true))])),
            None,
            None,
            TsiFlags::empty(),
        )
    }

    fn activate_muxer(muxer: &mut VsockMuxer) -> io::Result<()> {
        use crate::legacy::DummyIrqChip;
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let interrupt =
            InterruptTransport::new(DummyIrqChip::new().into(), "test-vsock".into()).unwrap();
        muxer.activate(mem, Arc::new(Mutex::new(VirtQueue::new(256))), interrupt)
    }

    fn wait_for_host_request(muxer: &VsockMuxer) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(rx) = muxer.rxq.lock().unwrap().pop() {
                assert!(matches!(
                    rx,
                    MuxerRx::OpRequest {
                        peer_port: TEST_PORT,
                        ..
                    }
                ));
                return;
            }
            assert!(
                Instant::now() < deadline,
                "host connection never reached the guest RX queue"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn host_listener_is_bound_before_activation_and_owned_until_drop() {
        let dir = TempDir::new().unwrap();
        let path = dir.as_path().join("host.sock");
        let muxer = listener_muxer(&path).unwrap();
        let _pending = UnixStream::connect(&path).unwrap();
        assert!(matches!(
            listener_muxer(&path),
            Err(VsockError::HostListener { .. })
        ));
        assert!(path.exists());
        drop(muxer);
        assert!(!path.exists());
        let replacement = listener_muxer(&path).unwrap();
        drop(replacement);
        assert!(!path.exists());
    }

    #[test]
    fn host_listener_failure_preserves_existing_paths_and_rolls_back_partial_bind() {
        let dir = TempDir::new().unwrap();
        let occupied = dir.as_path().join("occupied");
        std::fs::write(&occupied, b"owner").unwrap();
        assert!(listener_muxer(&occupied).is_err());
        assert_eq!(std::fs::read(&occupied).unwrap(), b"owner");
        assert!(listener_muxer(&dir.as_path().join("missing/host.sock")).is_err());
        let duplicate = dir.as_path().join("duplicate.sock");
        let result = VsockMuxer::new(
            TEST_CID,
            None,
            Some(HashMap::from([
                (5000, (duplicate.clone(), true)),
                (5001, (duplicate.clone(), true)),
            ])),
            None,
            None,
            TsiFlags::empty(),
        );
        assert!(matches!(result, Err(VsockError::HostListener { .. })));
        assert!(
            !duplicate.exists(),
            "failed construction leaked its first listener"
        );
    }

    #[test]
    fn host_listener_reactivation_rejects_replaced_path_without_unlinking_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.as_path().join("host.sock");
        let mut muxer = listener_muxer(&path).unwrap();
        activate_muxer(&mut muxer).unwrap();
        muxer.quiesce().unwrap();
        std::fs::remove_file(&path).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        assert_eq!(
            activate_muxer(&mut muxer).unwrap_err().kind(),
            io::ErrorKind::AddrNotAvailable
        );
        assert!(muxer.proxy_map.read().unwrap().is_empty());
        assert!(muxer.muxer_thread.is_none());
        drop(muxer);
        let _client = UnixStream::connect(&path).unwrap();
        replacement.accept().unwrap();
        drop(replacement);
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn host_listener_activation_rolls_back_failed_poll_registration() {
        let dir = TempDir::new().unwrap();
        let path = dir.as_path().join("host.sock");
        let mut muxer = listener_muxer(&path).unwrap();
        let fd = muxer.host_listeners[&TEST_PORT].as_raw_fd();
        muxer
            .epoll
            .ctl(
                ControlOperation::Add,
                fd,
                &EpollEvent::new(EventSet::IN, 123),
            )
            .unwrap();
        assert!(activate_muxer(&mut muxer).is_err());
        assert!(muxer.proxy_map.read().unwrap().is_empty());
        assert!(muxer.muxer_thread.is_none());
        assert!(muxer.reaper_thread.is_none());
        assert!(muxer.mem.is_none());
        activate_muxer(&mut muxer).unwrap();
        let _client = UnixStream::connect(&path).unwrap();
        wait_for_host_request(&muxer);
    }

    #[test]
    fn host_listener_accepts_after_every_quiescence_and_drop_joins_workers() {
        let (done, result) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let dir = TempDir::new().unwrap();
            let path = dir.as_path().join("host.sock");
            let mut muxer = listener_muxer(&path).unwrap();
            for _ in 0..8 {
                activate_muxer(&mut muxer).unwrap();
                assert_eq!(
                    activate_muxer(&mut muxer).unwrap_err().kind(),
                    io::ErrorKind::AlreadyExists
                );
                let _client = UnixStream::connect(&path).unwrap();
                wait_for_host_request(&muxer);
                muxer.quiesce().unwrap();
                assert!(path.exists());
                assert!(muxer.proxy_map.read().unwrap().is_empty());
            }
            activate_muxer(&mut muxer).unwrap();
            drop(muxer);
            assert!(!path.exists());
            done.send(()).unwrap();
        });
        result
            .recv_timeout(Duration::from_secs(5))
            .expect("listener workers failed to stop");
        worker.join().unwrap();
    }

    #[test]
    fn dropping_one_launch_keeps_other_listener_usable() {
        let dir = TempDir::new().unwrap();
        let first_path = dir.as_path().join("first.sock");
        let second_path = dir.as_path().join("second.sock");
        let first = listener_muxer(&first_path).unwrap();
        let mut second = listener_muxer(&second_path).unwrap();
        activate_muxer(&mut second).unwrap();
        drop(first);
        assert!(!first_path.exists());
        let _client = UnixStream::connect(&second_path).unwrap();
        wait_for_host_request(&second);
    }

    #[test]
    fn guest_tsi_create_cannot_replace_host_listener() {
        let dir = TempDir::new().unwrap();
        let path = dir.as_path().join("host.sock");
        let mut muxer = listener_muxer(&path).unwrap();
        muxer.tsi_flags = TsiFlags::HIJACK_INET | TsiFlags::HIJACK_UNIX;
        activate_muxer(&mut muxer).unwrap();
        let mem = muxer.mem.as_ref().unwrap().clone();
        let id = ((TEST_PORT as u64) << 32) | (defs::TSI_PROXY_PORT as u64);
        let listener_fd = muxer.host_listeners[&TEST_PORT].as_raw_fd();
        for family in [libc::AF_INET, libc::AF_UNIX] {
            for kind in [defs::SOCK_STREAM, defs::SOCK_DGRAM] {
                let _ = tx_packet(&mem, TEST_CID, uapi::VSOCK_HOST_CID, uapi::VSOCK_OP_RW);
                mem.write_obj(defs::TSI_PROXY_CREATE.to_le(), GuestAddress(HEADER.0 + 20))
                    .unwrap();
                mem.write_obj(8u32.to_le(), GuestAddress(HEADER.0 + 24))
                    .unwrap();
                mem.write_obj(
                    Descriptor {
                        addr: HEADER.0,
                        len: VSOCK_PKT_HDR_SIZE as u32,
                        flags: 1,
                        next: 1,
                    },
                    GuestAddress(0x1000),
                )
                .unwrap();
                mem.write_obj(
                    Descriptor {
                        addr: 0x3000,
                        len: 8,
                        flags: 0,
                        next: 0,
                    },
                    GuestAddress(0x1010),
                )
                .unwrap();
                mem.write_obj(TEST_PORT.to_le(), GuestAddress(0x3000))
                    .unwrap();
                mem.write_obj((family as u16).to_le(), GuestAddress(0x3004))
                    .unwrap();
                mem.write_obj((kind as u16).to_le(), GuestAddress(0x3006))
                    .unwrap();
                let head = DescriptorChain::checked_new(&mem, GuestAddress(0x1000), 2, 0).unwrap();
                let pkt = VsockPacket::from_tx_virtq_head(&head).unwrap();
                muxer.send_dgram_pkt(&pkt).unwrap();
                let map = muxer.proxy_map.read().unwrap();
                let listener = map[&id].lock().unwrap();
                assert_eq!(listener.pollable(), listener_fd);
                assert_eq!(
                    listener.status(),
                    super::super::proxy::ProxyStatus::WaitingOnAccept
                );
            }
        }
        let _client = UnixStream::connect(&path).unwrap();
        wait_for_host_request(&muxer);
    }

    #[test]
    fn guest_stream_packets_cannot_release_or_panic_host_listener() {
        let dir = TempDir::new().unwrap();
        let path = dir.as_path().join("host.sock");
        let mut muxer = listener_muxer(&path).unwrap();
        activate_muxer(&mut muxer).unwrap();
        let mem = muxer.mem.as_ref().unwrap().clone();
        let id = ((TEST_PORT as u64) << 32) | (defs::TSI_PROXY_PORT as u64);
        for op in 0..=uapi::VSOCK_OP_CREDIT_UPDATE {
            let _ = tx_packet(&mem, TEST_CID, uapi::VSOCK_HOST_CID, op);
            mem.write_obj(TEST_PORT.to_le(), GuestAddress(HEADER.0 + 16))
                .unwrap();
            mem.write_obj(defs::TSI_PROXY_PORT.to_le(), GuestAddress(HEADER.0 + 20))
                .unwrap();
            let head = DescriptorChain::checked_new(&mem, GuestAddress(0x1000), 1, 0).unwrap();
            let pkt = VsockPacket::from_tx_virtq_head(&head).unwrap();
            muxer.send_stream_pkt(&pkt).unwrap();
            assert!(muxer.proxy_map.read().unwrap().contains_key(&id));
        }
        let _client = UnixStream::connect(&path).unwrap();
        wait_for_host_request(&muxer);
    }

    #[derive(Default)]
    struct RecordingBackend {
        streams: Mutex<Vec<VsockConnectRequest>>,
        datagrams: Mutex<Vec<VsockDatagramPeer>>,
    }

    impl VsockPortBackend for RecordingBackend {
        fn connect(
            &self,
            request: VsockConnectRequest,
            _notifier: VsockNotifier,
        ) -> io::Result<Box<dyn VsockStreamBackend>> {
            self.streams.lock().unwrap().push(request);
            Err(io::ErrorKind::ConnectionRefused.into())
        }
    }

    impl VsockDatagramPortBackend for RecordingBackend {
        fn open_peer(
            &self,
            peer: VsockDatagramPeer,
            _notifier: VsockNotifier,
        ) -> io::Result<Box<dyn VsockDatagramBackend>> {
            self.datagrams.lock().unwrap().push(peer);
            Err(io::ErrorKind::ConnectionRefused.into())
        }
    }

    fn test_muxer(mem: &GuestMemoryMmap, backend: &Arc<RecordingBackend>) -> VsockMuxer {
        test_muxer_with_cid(mem, backend, TEST_CID)
    }

    fn test_muxer_with_cid(
        mem: &GuestMemoryMmap,
        backend: &Arc<RecordingBackend>,
        cid: u64,
    ) -> VsockMuxer {
        let mut muxer = VsockMuxer::new(
            cid,
            None,
            None,
            Some(HashMap::from([(
                TEST_PORT,
                backend.clone() as Arc<dyn VsockPortBackend>,
            )])),
            Some(HashMap::from([(
                TEST_PORT,
                backend.clone() as Arc<dyn VsockDatagramPortBackend>,
            )])),
            TsiFlags::empty(),
        )
        .unwrap();
        muxer.mem = Some(mem.clone());
        muxer.queue = Some(Arc::new(Mutex::new(VirtQueue::new(256))));
        muxer
    }

    proptest! {
        #[test]
        fn foreign_packet_sequences_preserve_authoritative_backend_identity(
            cid in 3u32..u32::MAX,
            packets in proptest::collection::vec((1u64..=u64::MAX, any::<u16>()), 1..25),
        ) {
            let cid = u64::from(cid);
            let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
            let backend = Arc::new(RecordingBackend::default());
            let mut muxer = test_muxer_with_cid(&mem, &backend, cid);
            for (difference, op) in packets {
                // Construct an unequal identity without filtering shrunk inputs.
                let pkt = tx_packet(&mem, cid ^ difference, uapi::VSOCK_HOST_CID, op);
                muxer.send_stream_pkt(&pkt).unwrap();
                muxer.send_dgram_pkt(&pkt).unwrap();
                prop_assert!(backend.streams.lock().unwrap().is_empty());
                prop_assert!(backend.datagrams.lock().unwrap().is_empty());
                prop_assert!(muxer.rxq.lock().unwrap().is_empty());
                prop_assert!(muxer.proxy_map.read().unwrap().is_empty());
                prop_assert!(muxer.dgram_peer_map.lock().unwrap().is_empty());
            }
            let request = tx_packet(&mem, cid, uapi::VSOCK_HOST_CID, uapi::VSOCK_OP_REQUEST);
            muxer.send_stream_pkt(&request).unwrap();
            let datagram = tx_packet(&mem, cid, uapi::VSOCK_HOST_CID, uapi::VSOCK_OP_RW);
            muxer.send_dgram_pkt(&datagram).unwrap();
            let streams = backend.streams.lock().unwrap();
            prop_assert_eq!(streams.as_slice(), &[VsockConnectRequest {
                guest_cid: cid, guest_port: 4000, host_port: TEST_PORT,
            }]);
            let datagrams = backend.datagrams.lock().unwrap();
            prop_assert_eq!(datagrams.as_slice(), &[VsockDatagramPeer {
                guest_cid: cid, guest_port: 4000, host_port: TEST_PORT,
            }]);
        }
    }

    fn tx_packet(mem: &GuestMemoryMmap, src_cid: u64, dst_cid: u64, op: u16) -> VsockPacket {
        let table = GuestAddress(0x1000);
        mem.write_obj(
            Descriptor {
                addr: HEADER.0,
                len: VSOCK_PKT_HDR_SIZE as u32,
                flags: 0,
                next: 0,
            },
            table,
        )
        .unwrap();
        let mut header = [0; VSOCK_PKT_HDR_SIZE];
        header[0..8].copy_from_slice(&src_cid.to_le_bytes());
        header[8..16].copy_from_slice(&dst_cid.to_le_bytes());
        header[16..20].copy_from_slice(&4000u32.to_le_bytes());
        header[20..24].copy_from_slice(&TEST_PORT.to_le_bytes());
        header[30..32].copy_from_slice(&op.to_le_bytes());
        mem.write_slice(&header, HEADER).unwrap();
        let head = DescriptorChain::checked_new(mem, table, 1, 0).unwrap();
        VsockPacket::from_tx_virtq_head(&head).unwrap()
    }

    #[test]
    fn foreign_cids_never_reach_stream_or_datagram_backends() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let backend = Arc::new(RecordingBackend::default());
        let mut muxer = test_muxer(&mem, &backend);
        for cid in [
            0,
            1,
            2,
            3,
            TEST_CID - 1,
            TEST_CID + 1,
            u32::MAX as u64,
            u64::MAX,
        ] {
            for op in 0..=uapi::VSOCK_OP_CREDIT_UPDATE {
                let pkt = tx_packet(&mem, cid, uapi::VSOCK_HOST_CID, op);
                muxer.send_stream_pkt(&pkt).unwrap();
                muxer.send_dgram_pkt(&pkt).unwrap();
            }
        }
        assert!(backend.streams.lock().unwrap().is_empty());
        assert!(backend.datagrams.lock().unwrap().is_empty());
        assert!(muxer.rxq.lock().unwrap().is_empty());
        assert!(muxer.proxy_map.read().unwrap().is_empty());
        assert!(muxer.dgram_peer_map.lock().unwrap().is_empty());

        // A rejected packet does not poison the next valid connection.
        let request = tx_packet(&mem, TEST_CID, uapi::VSOCK_HOST_CID, uapi::VSOCK_OP_REQUEST);
        muxer.send_stream_pkt(&request).unwrap();
        let datagram = tx_packet(&mem, TEST_CID, uapi::VSOCK_HOST_CID, uapi::VSOCK_OP_RW);
        muxer.send_dgram_pkt(&datagram).unwrap();
        assert_eq!(
            *backend.streams.lock().unwrap(),
            vec![VsockConnectRequest {
                guest_cid: TEST_CID,
                guest_port: 4000,
                host_port: TEST_PORT,
            }]
        );
        assert_eq!(
            *backend.datagrams.lock().unwrap(),
            vec![VsockDatagramPeer {
                guest_cid: TEST_CID,
                guest_port: 4000,
                host_port: TEST_PORT,
            }]
        );
    }

    #[test]
    fn changed_guest_headers_cannot_retarget_accepted_packets() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let backend = Arc::new(RecordingBackend::default());
        let mut muxer = test_muxer(&mem, &backend);
        for op in [uapi::VSOCK_OP_REQUEST, uapi::VSOCK_OP_RW] {
            let pkt = tx_packet(&mem, TEST_CID, uapi::VSOCK_HOST_CID, op);
            mem.write_slice(&[0xff; VSOCK_PKT_HDR_SIZE], HEADER)
                .unwrap();
            if op == uapi::VSOCK_OP_REQUEST {
                muxer.send_stream_pkt(&pkt).unwrap();
            } else {
                muxer.send_dgram_pkt(&pkt).unwrap();
            }
        }
        let streams = backend.streams.lock().unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0],
            VsockConnectRequest {
                guest_cid: TEST_CID,
                guest_port: 4000,
                host_port: TEST_PORT,
            }
        );
        let datagrams = backend.datagrams.lock().unwrap();
        assert_eq!(datagrams.len(), 1);
        assert_eq!(
            datagrams[0],
            VsockDatagramPeer {
                guest_cid: TEST_CID,
                guest_port: 4000,
                host_port: TEST_PORT,
            }
        );
    }

    #[test]
    fn non_host_destinations_never_reach_backends() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let backend = Arc::new(RecordingBackend::default());
        let mut muxer = test_muxer(&mem, &backend);
        for cid in [0, 1, 3, TEST_CID, u64::MAX] {
            let pkt = tx_packet(&mem, TEST_CID, cid, uapi::VSOCK_OP_REQUEST);
            muxer.send_stream_pkt(&pkt).unwrap();
            let pkt = tx_packet(&mem, TEST_CID, cid, uapi::VSOCK_OP_RW);
            muxer.send_dgram_pkt(&pkt).unwrap();
        }
        assert!(backend.streams.lock().unwrap().is_empty());
        assert!(backend.datagrams.lock().unwrap().is_empty());
    }

    #[test]
    fn datagram_peer_limit_evicts_the_least_recently_used_peer_per_port() {
        let muxer = VsockMuxer::new(3, None, None, None, None, TsiFlags::empty()).unwrap();
        {
            let mut peers = muxer.dgram_peer_map.lock().unwrap();
            for source_port in 1..=MAX_DGRAM_PEERS_PER_PORT as u32 {
                peers.insert(
                    (source_port, 5000),
                    DgramPeerEntry {
                        id: source_port as u64,
                        last_used: source_port as u64,
                    },
                );
            }
            peers.insert(
                (1, 6000),
                DgramPeerEntry {
                    id: u32::MAX as u64,
                    last_used: 0,
                },
            );
        }

        muxer.evict_oldest_dgram_peer(5000);

        let peers = muxer.dgram_peer_map.lock().unwrap();
        assert!(!peers.contains_key(&(1, 5000)));
        assert_eq!(
            peers.keys().filter(|(_, port)| *port == 5000).count(),
            MAX_DGRAM_PEERS_PER_PORT - 1
        );
        assert!(peers.contains_key(&(1, 6000)));
    }
}
