// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use utils::byte_order;
use utils::eventfd::EventFd;
use vm_memory::GuestMemoryMmap;

use super::super::{
    ActivateError, ActivateResult, DeviceQueue, DeviceState, Queue as VirtQueue, QueueConfig,
    VirtioDevice, VirtioStateError, VmmExitObserver,
};
use super::muxer::VsockMuxer;
use super::packet::VsockPacket;
use super::TsiFlags;
use super::{defs, defs::uapi};
use super::{VsockDatagramPortBackend, VsockPortBackend};
use crate::virtio::InterruptTransport;

pub(crate) const RXQ_INDEX: usize = 0;
pub(crate) const TXQ_INDEX: usize = 1;
pub(crate) const EVQ_INDEX: usize = 2;

/// The virtio features supported by our vsock device:
/// - VIRTIO_F_VERSION_1: the device conforms to at least version 1.0 of the VirtIO spec.
/// - VIRTIO_F_IN_ORDER: the device returns used buffers in the same order that the driver makes
///   them available.
pub(crate) const AVAIL_FEATURES: u64 = (1 << uapi::VIRTIO_F_VERSION_1 as u64)
    | (1 << uapi::VIRTIO_F_IN_ORDER as u64)
    | if cfg!(unix) {
        1 << uapi::VIRTIO_VSOCK_F_DGRAM
    } else {
        0
    };

pub struct Vsock {
    cid: u64,
    pub(crate) muxer: VsockMuxer,
    pub(crate) queue_rx: Option<Arc<Mutex<VirtQueue>>>,
    pub(crate) queue_tx: Option<Arc<Mutex<VirtQueue>>>,
    pub(crate) queue_ev: Option<VirtQueue>,
    // Queue events are stored separately for event handling.
    pub(crate) queue_events: Vec<Arc<EventFd>>,
    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,
    pub(crate) activate_evt: EventFd,
    pub(crate) device_state: DeviceState,
}

impl Vsock {
    /// Create a new virtio-vsock device with the given VM CID.
    pub fn new(
        cid: u64,
        host_port_map: Option<HashMap<u16, u16>>,
        unix_ipc_port_map: Option<HashMap<u32, (PathBuf, bool)>>,
        custom_port_map: Option<HashMap<u32, Arc<dyn VsockPortBackend>>>,
        custom_dgram_port_map: Option<HashMap<u32, Arc<dyn VsockDatagramPortBackend>>>,
        tsi_flags: TsiFlags,
    ) -> super::Result<Vsock> {
        let muxer = VsockMuxer::new(
            cid,
            host_port_map,
            unix_ipc_port_map,
            custom_port_map,
            custom_dgram_port_map,
            tsi_flags,
        );
        #[cfg(unix)]
        let muxer = muxer?;
        Ok(Vsock {
            cid,
            muxer,
            queue_rx: None,
            queue_tx: None,
            queue_ev: None,
            queue_events: Vec::new(),
            avail_features: AVAIL_FEATURES,
            acked_features: 0,
            activate_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK)
                .map_err(super::VsockError::EventFd)?,
            device_state: DeviceState::Inactive,
        })
    }

    pub fn id(&self) -> &str {
        defs::VSOCK_DEV_ID
    }

    pub fn cid(&self) -> u64 {
        self.cid
    }

    /// Walk the driver-provided RX queue buffers and attempt to fill them up with any data that we
    /// have pending. Return `true` if descriptors have been added to the used ring, and `false`
    /// otherwise.
    pub fn process_stream_rx(&mut self) -> bool {
        debug!("process_stream_rx()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            // This should never happen, it's been already validated in the event handler.
            DeviceState::Inactive => unreachable!(),
        };

        let mut have_used = false;
        let mut needs_backend_kick = false;

        debug!("process_rx before while");
        let queue_rx = self
            .queue_rx
            .as_ref()
            .expect("queue_rx should exist when activated");
        let mut queue_rx = queue_rx.lock().unwrap();
        while let Some(head) = queue_rx.pop(mem) {
            debug!("process_rx inside while");
            let used_len = match VsockPacket::from_rx_virtq_head(&head) {
                Ok(mut pkt) => {
                    if self.muxer.recv_pkt(&mut pkt).is_ok() {
                        pkt.hdr().len() as u32 + pkt.len()
                    } else {
                        // We are using a consuming iterator over the virtio buffers, so, if we can't
                        // fill in this buffer, we'll need to undo the last iterator step.
                        queue_rx.undo_pop();
                        needs_backend_kick = true;
                        break;
                    }
                }
                Err(e) => {
                    warn!("RX queue error: {e:?}");
                    0
                }
            };

            debug!("process_rx: something to queue");
            have_used = true;
            if let Err(e) = queue_rx.add_used(mem, head.index, used_len) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }

        // Backends can need a retry when the guest has just supplied receive
        // capacity. Drop the virtqueue lock before taking proxy locks so the
        // muxer thread cannot deadlock in the opposite proxy -> queue order.
        drop(queue_rx);
        if needs_backend_kick {
            self.muxer.kick_backends();
        }

        have_used
    }

    /// Walk the driver-provided TX queue buffers, package them up as vsock packets, and process
    /// them. Return `true` if descriptors have been added to the used ring, and `false` otherwise.
    pub fn process_stream_tx(&mut self) -> bool {
        debug!("process_stream_tx()");
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem,
            // This should never happen, it's been already validated in the event handler.
            DeviceState::Inactive => unreachable!(),
        };

        let mut have_used = false;

        let queue_tx = self
            .queue_tx
            .as_ref()
            .expect("queue_tx should exist when activated");
        let mut queue_tx = queue_tx.lock().unwrap();
        while let Some(head) = queue_tx.pop(mem) {
            let pkt = match VsockPacket::from_tx_virtq_head(&head) {
                Ok(pkt) => pkt,
                Err(e) => {
                    error!("error reading TX packet: {e:?}");
                    have_used = true;
                    if let Err(e) = queue_tx.add_used(mem, head.index, 0) {
                        error!("failed to add used elements to the queue: {e:?}");
                    }
                    continue;
                }
            };

            if pkt.type_() == uapi::VSOCK_TYPE_DGRAM {
                debug!("process_stream_tx() is DGRAM");
                if self.muxer.send_dgram_pkt(&pkt).is_err() {
                    queue_tx.undo_pop();
                    break;
                }
            } else {
                debug!("process_stream_tx() is STREAM");
                if self.muxer.send_stream_pkt(&pkt).is_err() {
                    queue_tx.undo_pop();
                    break;
                }
            }

            have_used = true;
            if let Err(e) = queue_tx.add_used(mem, head.index, 0) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }

        have_used
    }
}

impl VirtioDevice for Vsock {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features
    }

    fn device_type(&self) -> u32 {
        uapi::VIRTIO_ID_VSOCK
    }

    fn device_name(&self) -> &str {
        "vsock"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &defs::QUEUE_CONFIG
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        match offset {
            0 if data.len() == 8 => byte_order::write_le_u64(data, self.cid()),
            0 if data.len() == 4 => {
                byte_order::write_le_u32(data, (self.cid() & 0xffff_ffff) as u32)
            }
            4 if data.len() == 4 => {
                byte_order::write_le_u32(data, ((self.cid() >> 32) & 0xffff_ffff) as u32)
            }
            _ => warn!(
                "virtio-vsock received invalid read request of {} bytes at offset {}",
                data.len(),
                offset
            ),
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        warn!(
            "guest driver attempted to write device config (offset={:x}, len={:x})",
            offset,
            data.len()
        );
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        if self.is_activated() {
            return Err(ActivateError::BadActivate);
        }
        if queues.len() != defs::NUM_QUEUES {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                defs::NUM_QUEUES,
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        // Store queue events for event handling.
        self.queue_events = queues.iter().map(|dq| dq.event.clone()).collect();

        // Extract queues from DeviceQueues and wrap in Arc<Mutex<>>.
        let mut queues_vec: Vec<VirtQueue> = queues.into_iter().map(|dq| dq.queue).collect();
        // Note: EVQ (index 2) is currently unused, we just take it to maintain the vec.
        let evq = queues_vec.pop().unwrap();
        let tx_queue = queues_vec.pop().unwrap();
        let rx_queue = queues_vec.pop().unwrap();

        self.queue_tx = Some(Arc::new(Mutex::new(tx_queue)));
        self.queue_rx = Some(Arc::new(Mutex::new(rx_queue)));
        self.queue_ev = Some(evq);
        if let Err(error) = self.muxer.activate(
            mem.clone(),
            self.queue_rx.clone().unwrap(),
            interrupt.clone(),
        ) {
            self.queue_rx = None;
            self.queue_tx = None;
            self.queue_ev = None;
            self.queue_events.clear();
            return Err(ActivateError::EpollCtl(error));
        }

        self.device_state = DeviceState::Activated(mem, interrupt);
        if self.activate_evt.write(1).is_err() {
            let _ = self.muxer.quiesce();
            self.device_state = DeviceState::Inactive;
            self.queue_rx = None;
            self.queue_tx = None;
            self.queue_ev = None;
            self.queue_events.clear();
            error!("Cannot write to activate_evt");
            return Err(ActivateError::BadActivate);
        }

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn reset(&mut self) -> bool {
        self.quiesce().is_ok()
    }

    fn supports_quiesce(&self) -> bool {
        true
    }

    fn quiesce(&mut self) -> Result<Vec<DeviceQueue>, VirtioStateError> {
        if !self.device_state.is_activated() {
            return Err(VirtioStateError::InvalidLifecycle(
                "vsock must be activated before quiescence",
            ));
        }

        // Stop muxer, reaper, and platform time-sync writers before reclaiming queue ownership.
        // Active streams intentionally reset; agent and application protocols reconnect after
        // resume rather than serializing host socket handles.
        self.muxer.quiesce().map_err(VirtioStateError::Device)?;
        let rx = take_shared_queue(
            self.queue_rx
                .take()
                .ok_or(VirtioStateError::InvalidLifecycle(
                    "vsock RX queue is missing",
                ))?,
            "vsock RX queue still has an owner after muxer stop",
        )?;
        let tx = take_shared_queue(
            self.queue_tx
                .take()
                .ok_or(VirtioStateError::InvalidLifecycle(
                    "vsock TX queue is missing",
                ))?,
            "vsock TX queue still has an owner after muxer stop",
        )?;
        let ev = self
            .queue_ev
            .take()
            .ok_or(VirtioStateError::InvalidLifecycle(
                "vsock event queue is missing",
            ))?;
        self.device_state = DeviceState::Inactive;
        Ok(vec![
            DeviceQueue::new(rx, self.queue_events[RXQ_INDEX].clone()),
            DeviceQueue::new(tx, self.queue_events[TXQ_INDEX].clone()),
            DeviceQueue::new(ev, self.queue_events[EVQ_INDEX].clone()),
        ])
    }
}

impl VmmExitObserver for Vsock {
    fn on_vmm_exit(&mut self, _exit_code: i32) {
        // Normal VMM shutdown uses _exit(), so destructors alone do not run.
        #[cfg(unix)]
        let result = self.muxer.retire();
        #[cfg(windows)]
        let result = self.muxer.quiesce();
        if let Err(error) = result {
            error!("failed to stop vsock transport on VMM exit: {error}");
        }
    }
}

fn take_shared_queue(
    queue: Arc<Mutex<VirtQueue>>,
    error: &'static str,
) -> Result<VirtQueue, VirtioStateError> {
    Arc::try_unwrap(queue)
        .map_err(|_| VirtioStateError::Incompatible(error.to_string()))?
        .into_inner()
        .map_err(|_| VirtioStateError::Incompatible("vsock queue mutex is poisoned".to_string()))
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vm_memory::GuestAddress;

    use crate::legacy::DummyIrqChip;
    use crate::virtio::{DeviceQueue, InterruptTransport, Queue};

    use super::*;

    fn queues() -> Vec<DeviceQueue> {
        defs::QUEUE_CONFIG
            .iter()
            .map(|config| {
                DeviceQueue::new(Queue::new(config.size), Arc::new(EventFd::new(0).unwrap()))
            })
            .collect()
    }

    #[test]
    #[cfg(unix)]
    fn failed_host_listener_activation_is_not_announced() {
        let dir = utils::tempdir::TempDir::new().unwrap();
        let path = dir.as_path().join("host.sock");
        let mut vsock = Vsock::new(
            3,
            None,
            Some(HashMap::from([(5000, (path.clone(), true))])),
            None,
            None,
            TsiFlags::empty(),
        )
        .unwrap();
        std::fs::remove_file(&path).unwrap();
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1_0000)]).unwrap();
        let interrupt =
            InterruptTransport::new(DummyIrqChip::new().into(), "test-vsock".into()).unwrap();
        assert!(vsock.activate(mem, interrupt, queues()).is_err());
        assert!(!vsock.is_activated());
        assert_eq!(
            vsock.activate_evt.read().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(vsock.queue_rx.is_none());
        assert!(vsock.queue_tx.is_none());
        assert!(vsock.queue_ev.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn exit_observer_releases_listener_before_process_exit() {
        let dir = utils::tempdir::TempDir::new().unwrap();
        let path = dir.as_path().join("host.sock");
        for active in [false, true] {
            let mut vsock = Vsock::new(
                3,
                None,
                Some(HashMap::from([(5000, (path.clone(), true))])),
                None,
                None,
                TsiFlags::empty(),
            )
            .unwrap();
            let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1_0000)]).unwrap();
            let interrupt =
                InterruptTransport::new(DummyIrqChip::new().into(), "test-vsock".into()).unwrap();
            if active {
                vsock
                    .activate(mem.clone(), interrupt.clone(), queues())
                    .unwrap();
            }
            assert!(path.exists());
            vsock.on_vmm_exit(0);
            assert!(!path.exists());
            vsock.on_vmm_exit(0);
            assert!(vsock
                .muxer
                .activate(mem, Arc::new(Mutex::new(Queue::new(256))), interrupt)
                .is_err());
        }
    }

    #[test]
    fn vsock_quiesce_joins_background_workers_and_is_reactivatable() {
        let (completed, result) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            check_repeated_quiescence();
            completed.send(()).unwrap();
        });
        result
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("vsock workers must stop promptly on every activation");
        worker.join().unwrap();
    }

    fn check_repeated_quiescence() {
        let mut vsock = Vsock::new(3, None, None, None, None, TsiFlags::empty()).unwrap();
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1_0000)]).unwrap();
        let interrupt =
            InterruptTransport::new(DummyIrqChip::new().into(), "test-vsock".into()).unwrap();

        let mut queues = queues();
        for _ in 0..8 {
            vsock
                .activate(mem.clone(), interrupt.clone(), queues)
                .unwrap();
            queues = vsock.quiesce().unwrap();
            assert_eq!(queues.len(), defs::NUM_QUEUES);
            assert!(!vsock.is_activated());
        }
    }
}
