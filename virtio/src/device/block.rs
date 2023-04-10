// Copyright (c) 2020 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

use std::cmp;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::mem::size_of;
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::VirtioError;
use crate::{
    iov_discard_back, iov_discard_front, iov_to_buf, report_virtio_error, virtio_has_feature,
    Element, Queue, VirtioDevice, VirtioInterrupt, VirtioInterruptType, VirtioTrace,
    VIRTIO_BLK_F_DISCARD, VIRTIO_BLK_F_FLUSH, VIRTIO_BLK_F_MQ, VIRTIO_BLK_F_RO,
    VIRTIO_BLK_F_SEG_MAX, VIRTIO_BLK_F_WRITE_ZEROES, VIRTIO_BLK_ID_BYTES, VIRTIO_BLK_S_IOERR,
    VIRTIO_BLK_S_OK, VIRTIO_BLK_S_UNSUPP, VIRTIO_BLK_T_DISCARD, VIRTIO_BLK_T_FLUSH,
    VIRTIO_BLK_T_GET_ID, VIRTIO_BLK_T_IN, VIRTIO_BLK_T_OUT, VIRTIO_BLK_T_WRITE_ZEROES,
    VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP, VIRTIO_F_RING_EVENT_IDX, VIRTIO_F_RING_INDIRECT_DESC,
    VIRTIO_F_VERSION_1, VIRTIO_TYPE_BLOCK,
};
use address_space::{AddressSpace, GuestAddress};
use anyhow::{anyhow, bail, Context, Result};
use byteorder::{ByteOrder, LittleEndian};
use log::{error, warn};
use machine_manager::config::{BlkDevConfig, ConfigCheck, DriveFile, VmConfig};
use machine_manager::event_loop::{register_event_helper, unregister_event_helper, EventLoop};
use migration::{
    migration::Migratable, DeviceStateDesc, FieldDesc, MigrationHook, MigrationManager,
    StateTransfer,
};
use migration_derive::{ByteCode, Desc};
use util::aio::{
    iov_from_buf_direct, iov_to_buf_direct, raw_datasync, Aio, AioCb, AioEngine, Iovec, OpCode,
    WriteZeroesState,
};
use util::byte_code::ByteCode;
use util::leak_bucket::LeakBucket;
use util::loop_context::{
    read_fd, EventNotifier, EventNotifierHelper, NotifierCallback, NotifierOperation,
};
use util::num_ops::read_u32;
use util::offset_of;
use vmm_sys_util::{epoll::EventSet, eventfd::EventFd};
/// Number of virtqueues.
const QUEUE_NUM_BLK: usize = 1;
/// Used to compute the number of sectors.
const SECTOR_SHIFT: u8 = 9;
/// Size of a sector of the block device.
const SECTOR_SIZE: u64 = (0x01_u64) << SECTOR_SHIFT;
/// Size of the dummy block device.
const DUMMY_IMG_SIZE: u64 = 0;
/// Max number reqs of a merged request.
const MAX_NUM_MERGE_REQS: u16 = 32;
/// Max number iovs of a merged request.
const MAX_NUM_MERGE_IOVS: usize = 1024;
/// Max number bytes of a merged request.
const MAX_NUM_MERGE_BYTES: u64 = i32::MAX as u64;
/// Max time for every round of process queue.
const MAX_MILLIS_TIME_PROCESS_QUEUE: u16 = 100;
/// Max number sectors of per request.
const MAX_REQUEST_SECTORS: u32 = u32::MAX >> SECTOR_SHIFT;

type SenderConfig = (
    Option<Arc<File>>,
    u32,
    u32,
    u64,
    Option<String>,
    bool,
    AioEngine,
);

fn get_serial_num_config(serial_num: &str) -> Vec<u8> {
    let mut id_bytes = vec![0; VIRTIO_BLK_ID_BYTES as usize];
    let bytes_to_copy = cmp::min(serial_num.len(), VIRTIO_BLK_ID_BYTES as usize);

    let serial_bytes = serial_num.as_bytes();
    id_bytes[..bytes_to_copy].clone_from_slice(&serial_bytes[..bytes_to_copy]);
    id_bytes
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct RequestOutHeader {
    request_type: u32,
    io_prio: u32,
    sector: u64,
}

impl ByteCode for RequestOutHeader {}

/// The request of discard and write-zeroes use the same struct.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DiscardWriteZeroesSeg {
    /// The start sector for discard or write-zeroes.
    sector: u64,
    /// The number of sectors for discard or write-zeroes.
    num_sectors: u32,
    /// The flags used for this range.
    flags: u32,
}

impl ByteCode for DiscardWriteZeroesSeg {}

#[derive(Clone)]
pub struct AioCompleteCb {
    queue: Arc<Mutex<Queue>>,
    mem_space: Arc<AddressSpace>,
    /// The head of merged Request list.
    req: Rc<Request>,
    interrupt_cb: Arc<VirtioInterrupt>,
    driver_features: u64,
}

impl AioCompleteCb {
    fn new(
        queue: Arc<Mutex<Queue>>,
        mem_space: Arc<AddressSpace>,
        req: Rc<Request>,
        interrupt_cb: Arc<VirtioInterrupt>,
        driver_features: u64,
    ) -> Self {
        AioCompleteCb {
            queue,
            mem_space,
            req,
            interrupt_cb,
            driver_features,
        }
    }

    fn complete_request(&self, status: u8) -> Result<()> {
        let mut req = Some(self.req.as_ref());
        while let Some(req_raw) = req {
            self.complete_one_request(req_raw, status)?;
            req = req_raw.next.as_ref().as_ref();
        }
        Ok(())
    }

    fn complete_one_request(&self, req: &Request, status: u8) -> Result<()> {
        if let Err(ref e) = self.mem_space.write_object(&status, req.in_header) {
            bail!("Failed to write the status (blk io completion) {:?}", e);
        }

        let mut queue_lock = self.queue.lock().unwrap();
        queue_lock
            .vring
            .add_used(&self.mem_space, req.desc_index, req.in_len)
            .with_context(|| {
                format!(
                    "Failed to add used ring(blk io completion), index {}, len {}",
                    req.desc_index, req.in_len
                )
            })?;

        if queue_lock
            .vring
            .should_notify(&self.mem_space, self.driver_features)
        {
            (self.interrupt_cb)(&VirtioInterruptType::Vring, Some(&queue_lock), false)
                .with_context(|| {
                    VirtioError::InterruptTrigger("blk io completion", VirtioInterruptType::Vring)
                })?;
            self.trace_send_interrupt("Block".to_string());
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Request {
    desc_index: u16,
    out_header: RequestOutHeader,
    iovec: Vec<Iovec>,
    data_len: u64,
    in_len: u32,
    in_header: GuestAddress,
    /// Point to the next merged Request.
    next: Box<Option<Request>>,
}

impl Request {
    fn new(handler: &BlockIoHandler, elem: &mut Element, status: &mut u8) -> Result<Self> {
        if elem.out_iovec.is_empty() || elem.in_iovec.is_empty() {
            bail!(
                "Missed header for block request: out {} in {} desc num {}",
                elem.out_iovec.len(),
                elem.in_iovec.len(),
                elem.desc_num
            );
        }

        let mut out_header = RequestOutHeader::default();
        iov_to_buf(
            &handler.mem_space,
            &elem.out_iovec,
            out_header.as_mut_bytes(),
        )
        .and_then(|size| {
            if size < size_of::<RequestOutHeader>() {
                bail!("Invalid out header for block request: length {}", size);
            }
            Ok(())
        })?;
        out_header.request_type = LittleEndian::read_u32(out_header.request_type.as_bytes());
        out_header.sector = LittleEndian::read_u64(out_header.sector.as_bytes());

        let in_iov_elem = elem.in_iovec.last().unwrap();
        if in_iov_elem.len < 1 {
            bail!(
                "Invalid in header for block request: length {}",
                in_iov_elem.len
            );
        }
        // Note: addr plus len has been checked not overflow in virtqueue.
        let in_header = GuestAddress(in_iov_elem.addr.0 + in_iov_elem.len as u64 - 1);

        let mut request = Request {
            desc_index: elem.index,
            out_header,
            iovec: Vec::with_capacity(elem.desc_num as usize),
            data_len: 0,
            in_len: 0,
            in_header,
            next: Box::new(None),
        };

        // Count in_len before discard iovec.
        // We always write the last status byte, so count all in_iovs.
        // Note: in_iov and out_iov total len is no more than 1<<32, and
        // out_iov is more than 1, so in_len will not overflow.
        for in_iov in elem.in_iovec.iter() {
            request.in_len += in_iov.len;
        }

        match out_header.request_type {
            VIRTIO_BLK_T_IN
            | VIRTIO_BLK_T_GET_ID
            | VIRTIO_BLK_T_OUT
            | VIRTIO_BLK_T_DISCARD
            | VIRTIO_BLK_T_WRITE_ZEROES => {
                let data_iovec = match out_header.request_type {
                    VIRTIO_BLK_T_OUT | VIRTIO_BLK_T_DISCARD | VIRTIO_BLK_T_WRITE_ZEROES => {
                        iov_discard_front(&mut elem.out_iovec, size_of::<RequestOutHeader>() as u64)
                    }
                    // Otherwise discard the last "status" byte.
                    _ => iov_discard_back(&mut elem.in_iovec, 1),
                };
                if data_iovec.is_none() {
                    bail!("Empty data for block request");
                }
                for elem_iov in data_iovec.unwrap() {
                    if let Some(hva) = handler.mem_space.get_host_address(elem_iov.addr) {
                        let iov = Iovec {
                            iov_base: hva,
                            iov_len: u64::from(elem_iov.len),
                        };
                        request.iovec.push(iov);
                        // Note: elem_iov total len is no more than 1<<32.
                        request.data_len += u64::from(elem_iov.len);
                    } else {
                        bail!("Map desc base {:?} failed", elem_iov.addr);
                    }
                }
            }
            VIRTIO_BLK_T_FLUSH => (),
            others => {
                error!("Request type {} is not supported for block", others);
                *status = VIRTIO_BLK_S_UNSUPP;
            }
        }

        if !request.io_range_valid(handler.disk_sectors) {
            *status = VIRTIO_BLK_S_IOERR;
        }

        Ok(request)
    }

    fn execute(
        &self,
        iohandler: &mut BlockIoHandler,
        mut aiocb: AioCb<AioCompleteCb>,
    ) -> Result<()> {
        let mut req = Some(self);
        while let Some(req_raw) = req {
            for iov in req_raw.iovec.iter() {
                let iovec = Iovec {
                    iov_base: iov.iov_base,
                    iov_len: iov.iov_len,
                };
                aiocb.iovec.push(iovec);
                // Note: total len of each req is no more than 1<<32,
                // and reqs count is no more than 1024.
                aiocb.nbytes += iov.iov_len;
            }
            req = req_raw.next.as_ref().as_ref();
        }

        let request_type = self.out_header.request_type;
        if MigrationManager::is_active()
            && (request_type == VIRTIO_BLK_T_IN || request_type == VIRTIO_BLK_T_GET_ID)
        {
            // FIXME: mark dirty page needs to be managed by `AddressSpace` crate.
            for iov in aiocb.iovec.iter() {
                // Mark vmm dirty page manually if live migration is active.
                MigrationManager::mark_dirty_log(iov.iov_base, iov.iov_len);
            }
        }

        let aio = &mut iohandler.aio;
        let serial_num = &iohandler.serial_num;
        match request_type {
            VIRTIO_BLK_T_IN => {
                aiocb.opcode = OpCode::Preadv;
                aio.submit_request(aiocb)
                    .with_context(|| "Failed to process block request for reading")?;
            }
            VIRTIO_BLK_T_OUT => {
                aiocb.opcode = OpCode::Pwritev;
                aio.submit_request(aiocb)
                    .with_context(|| "Failed to process block request for writing")?;
            }
            VIRTIO_BLK_T_FLUSH => {
                aiocb.opcode = OpCode::Fdsync;
                aio.submit_request(aiocb)
                    .with_context(|| "Failed to process block request for flushing")?;
            }
            VIRTIO_BLK_T_GET_ID => {
                let serial = serial_num.clone().unwrap_or_else(|| String::from(""));
                let serial_vec = get_serial_num_config(&serial);
                let status = iov_from_buf_direct(&self.iovec, &serial_vec).map_or_else(
                    |e| {
                        error!("Failed to process block request for getting id, {:?}", e);
                        VIRTIO_BLK_S_IOERR
                    },
                    |_| VIRTIO_BLK_S_OK,
                );
                aiocb.iocompletecb.complete_request(status)?;
            }
            VIRTIO_BLK_T_DISCARD => {
                if !iohandler.discard {
                    error!("Device does not support discard");
                    return aiocb.iocompletecb.complete_request(VIRTIO_BLK_S_UNSUPP);
                }
                aiocb.opcode = OpCode::Discard;
                self.handle_discard_write_zeroes_req(iohandler, aiocb)?;
            }
            VIRTIO_BLK_T_WRITE_ZEROES => {
                if iohandler.write_zeroes == WriteZeroesState::Off {
                    error!("Device does not support write-zeroes");
                    return aiocb.iocompletecb.complete_request(VIRTIO_BLK_S_UNSUPP);
                }
                aiocb.opcode = OpCode::WriteZeroes;
                self.handle_discard_write_zeroes_req(iohandler, aiocb)?;
            }
            // The illegal request type has been handled in method new().
            _ => {}
        };
        Ok(())
    }

    fn handle_discard_write_zeroes_req(
        &self,
        iohandler: &mut BlockIoHandler,
        mut aiocb: AioCb<AioCompleteCb>,
    ) -> Result<()> {
        let size = size_of::<DiscardWriteZeroesSeg>() as u64;
        // Just support one segment per request.
        if self.data_len > size {
            error!("More than one discard or write-zeroes segment is not supported");
            return aiocb.iocompletecb.complete_request(VIRTIO_BLK_S_UNSUPP);
        }

        // Get and check the discard segment.
        let mut segment = DiscardWriteZeroesSeg::default();
        iov_to_buf_direct(&self.iovec, segment.as_mut_bytes()).and_then(|v| {
            if v as u64 == size {
                Ok(())
            } else {
                Err(anyhow!("Invalid discard segment size {}", v))
            }
        })?;
        let sector = LittleEndian::read_u64(segment.sector.as_bytes());
        let num_sectors = LittleEndian::read_u32(segment.num_sectors.as_bytes());
        if sector
            .checked_add(num_sectors as u64)
            .filter(|&off| off <= iohandler.disk_sectors)
            .is_none()
            || num_sectors > MAX_REQUEST_SECTORS
        {
            error!(
                "Invalid discard or write zeroes request, sector offset {}, num_sectors {}",
                sector, num_sectors
            );
            return aiocb.iocompletecb.complete_request(VIRTIO_BLK_S_IOERR);
        }
        let flags = LittleEndian::read_u32(segment.flags.as_bytes());
        if flags & !VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP != 0 {
            error!("Invalid unmap flags 0x{:x}", flags);
            return aiocb.iocompletecb.complete_request(VIRTIO_BLK_S_UNSUPP);
        }

        if aiocb.opcode == OpCode::Discard {
            if flags == VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP {
                error!("Discard request must not set unmap flags");
                return aiocb.iocompletecb.complete_request(VIRTIO_BLK_S_UNSUPP);
            }
        } else if aiocb.opcode == OpCode::WriteZeroes
            && flags == VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP
            && iohandler.discard
        {
            aiocb.write_zeroes_unmap = true;
        }

        aiocb.offset = (sector as usize) << SECTOR_SHIFT;
        aiocb.nbytes = (num_sectors as u64) << SECTOR_SHIFT;
        iohandler
            .aio
            .submit_request(aiocb)
            .with_context(|| "Failed to process block request for discard or write-zeroes")
    }

    fn io_range_valid(&self, disk_sectors: u64) -> bool {
        match self.out_header.request_type {
            VIRTIO_BLK_T_IN | VIRTIO_BLK_T_OUT => {
                if self.data_len % SECTOR_SIZE != 0 {
                    error!("Failed to process block request with size not aligned to 512B");
                    return false;
                }
                if self
                    .get_req_sector_num()
                    .checked_add(self.out_header.sector)
                    .filter(|&off| off <= disk_sectors)
                    .is_none()
                {
                    error!(
                        "offset {} invalid, disk sector {}",
                        self.out_header.sector, disk_sectors
                    );
                    return false;
                }
                true
            }
            _ => true,
        }
    }

    fn get_req_sector_num(&self) -> u64 {
        self.data_len / SECTOR_SIZE
    }
}

/// Control block of Block IO.
struct BlockIoHandler {
    /// The virtqueue.
    queue: Arc<Mutex<Queue>>,
    /// Eventfd of the virtqueue for IO event.
    queue_evt: Arc<EventFd>,
    /// The address space to which the block device belongs.
    mem_space: Arc<AddressSpace>,
    /// The image file opened by the block device.
    disk_image: Option<Arc<File>>,
    /// The align requirement of request(offset/len).
    pub req_align: u32,
    /// The align requirement of buffer(iova_base).
    pub buf_align: u32,
    /// The number of sectors of the disk image.
    disk_sectors: u64,
    /// Serial number of the block device.
    serial_num: Option<String>,
    /// If use direct access io.
    direct: bool,
    /// Aio context.
    aio: Box<Aio<AioCompleteCb>>,
    /// Bit mask of features negotiated by the backend and the frontend.
    driver_features: u64,
    /// The receiving half of Rust's channel to receive the image file.
    receiver: Receiver<SenderConfig>,
    /// Eventfd for config space update.
    update_evt: Arc<EventFd>,
    /// Device is broken or not.
    device_broken: Arc<AtomicBool>,
    /// Callback to trigger an interrupt.
    interrupt_cb: Arc<VirtioInterrupt>,
    /// thread name of io handler
    iothread: Option<String>,
    /// Using the leak bucket to implement IO limits
    leak_bucket: Option<LeakBucket>,
    /// Supporting discard or not.
    discard: bool,
    /// The write-zeroes state.
    write_zeroes: WriteZeroesState,
}

impl BlockIoHandler {
    fn merge_req_queue(&self, mut req_queue: Vec<Request>) -> Vec<Request> {
        req_queue.sort_by(|a, b| a.out_header.sector.cmp(&b.out_header.sector));

        let mut merge_req_queue = Vec::<Request>::new();
        let mut last_req: Option<&mut Request> = None;
        let mut merged_reqs = 0;
        let mut merged_iovs = 0;
        let mut merged_bytes = 0;

        for req in req_queue {
            let req_iovs = req.iovec.len();
            let req_bytes = req.data_len;
            let io = req.out_header.request_type == VIRTIO_BLK_T_IN
                || req.out_header.request_type == VIRTIO_BLK_T_OUT;
            let can_merge = match last_req {
                Some(ref req_ref) => {
                    io && req_ref.out_header.request_type == req.out_header.request_type
                        // Note: sector plus sector_num has been checked not overflow.
                        && (req_ref.out_header.sector + req_ref.get_req_sector_num() == req.out_header.sector)
                        && merged_reqs < MAX_NUM_MERGE_REQS
                        && merged_iovs + req_iovs <= MAX_NUM_MERGE_IOVS
                        && merged_bytes + req_bytes <= MAX_NUM_MERGE_BYTES
                }
                None => false,
            };

            if can_merge {
                let last_req_raw = last_req.unwrap();
                last_req_raw.next = Box::new(Some(req));
                last_req = last_req_raw.next.as_mut().as_mut();
                merged_reqs += 1;
                merged_iovs += req_iovs;
                merged_bytes += req_bytes;
            } else {
                merge_req_queue.push(req);
                last_req = merge_req_queue.last_mut();
                merged_reqs = 1;
                merged_iovs = req_iovs;
                merged_bytes = req_bytes;
            }
        }

        merge_req_queue
    }

    fn process_queue_internal(&mut self) -> Result<bool> {
        let mut req_queue = Vec::new();
        let mut done = false;

        loop {
            let mut queue = self.queue.lock().unwrap();
            let mut elem = queue
                .vring
                .pop_avail(&self.mem_space, self.driver_features)?;
            if elem.desc_num == 0 {
                break;
            }

            // limit io operations if iops is configured
            if let Some(lb) = self.leak_bucket.as_mut() {
                if let Some(ctx) = EventLoop::get_ctx(self.iothread.as_ref()) {
                    if lb.throttled(ctx, 1_u64) {
                        queue.vring.push_back();
                        break;
                    }
                };
            }

            // Init and put valid request into request queue.
            let mut status = VIRTIO_BLK_S_OK;
            let req = Request::new(self, &mut elem, &mut status)?;
            if status != VIRTIO_BLK_S_OK {
                let aiocompletecb = AioCompleteCb::new(
                    self.queue.clone(),
                    self.mem_space.clone(),
                    Rc::new(req),
                    self.interrupt_cb.clone(),
                    self.driver_features,
                );
                // unlock queue, because it will be hold below.
                drop(queue);
                aiocompletecb.complete_request(status)?;
                continue;
            }
            // Avoid bogus guest stuck IO thread.
            if req_queue.len() >= queue.vring.actual_size() as usize {
                bail!("The front driver may be damaged, avail requests more than queue size");
            }
            req_queue.push(req);
            done = true;
        }

        if req_queue.is_empty() {
            return Ok(done);
        }

        let merge_req_queue = self.merge_req_queue(req_queue);
        for req in merge_req_queue.into_iter() {
            let req_rc = Rc::new(req);
            let aiocompletecb = AioCompleteCb::new(
                self.queue.clone(),
                self.mem_space.clone(),
                req_rc.clone(),
                self.interrupt_cb.clone(),
                self.driver_features,
            );
            if let Some(disk_img) = self.disk_image.as_ref() {
                let aiocb = AioCb {
                    direct: self.direct,
                    req_align: self.req_align,
                    buf_align: self.buf_align,
                    file_fd: disk_img.as_raw_fd(),
                    opcode: OpCode::Noop,
                    iovec: Vec::new(),
                    offset: (req_rc.out_header.sector << SECTOR_SHIFT) as usize,
                    nbytes: 0,
                    user_data: 0,
                    iocompletecb: aiocompletecb,
                    discard: self.discard,
                    write_zeroes: self.write_zeroes,
                    write_zeroes_unmap: false,
                };
                req_rc.execute(self, aiocb)?;
            } else {
                warn!("Failed to execute block request, disk_img not specified");
                aiocompletecb.complete_request(VIRTIO_BLK_S_IOERR)?;
            }
        }
        self.aio.flush_request()?;

        Ok(done)
    }

    fn process_queue_suppress_notify(&mut self) -> Result<bool> {
        let mut done = false;
        let start_time = Instant::now();

        while self
            .queue
            .lock()
            .unwrap()
            .vring
            .avail_ring_len(&self.mem_space)?
            != 0
        {
            // Do not stuck IO thread.
            let now = Instant::now();
            if (now - start_time).as_millis() > MAX_MILLIS_TIME_PROCESS_QUEUE as u128 {
                // Make sure we can come back.
                self.queue_evt.write(1)?;
                break;
            }

            self.queue.lock().unwrap().vring.suppress_queue_notify(
                &self.mem_space,
                self.driver_features,
                true,
            )?;

            done = self.process_queue_internal()?;

            self.queue.lock().unwrap().vring.suppress_queue_notify(
                &self.mem_space,
                self.driver_features,
                false,
            )?;

            // See whether we have been throttled.
            if let Some(lb) = self.leak_bucket.as_mut() {
                if let Some(ctx) = EventLoop::get_ctx(self.iothread.as_ref()) {
                    if lb.throttled(ctx, 0) {
                        break;
                    }
                }
            }
        }
        Ok(done)
    }

    fn process_queue(&mut self) -> Result<bool> {
        self.trace_request("Block".to_string(), "to IO".to_string());
        let result = self.process_queue_suppress_notify();
        if result.is_err() {
            report_virtio_error(
                self.interrupt_cb.clone(),
                self.driver_features,
                &self.device_broken,
            );
        }
        result
    }

    fn complete_func(aiocb: &AioCb<AioCompleteCb>, ret: i64) -> Result<()> {
        let mut status = if ret < 0 {
            VIRTIO_BLK_S_IOERR
        } else {
            VIRTIO_BLK_S_OK
        };

        let complete_cb = &aiocb.iocompletecb;
        // When driver does not accept FLUSH feature, the device must be of
        // writethrough cache type, so flush data before updating used ring.
        if !virtio_has_feature(complete_cb.driver_features, VIRTIO_BLK_F_FLUSH)
            && aiocb.opcode == OpCode::Pwritev
            && ret >= 0
            && raw_datasync(aiocb.file_fd) < 0
        {
            error!("Failed to flush data before send response to guest.");
            status = VIRTIO_BLK_S_IOERR;
        }

        complete_cb.complete_request(status)
    }

    fn aio_complete_handler(&mut self) -> Result<bool> {
        self.aio.handle_complete().map_err(|e| {
            report_virtio_error(
                self.interrupt_cb.clone(),
                self.driver_features,
                &self.device_broken,
            );
            e
        })
    }

    fn update_evt_handler(&mut self) {
        let aio_engine;
        match self.receiver.recv() {
            Ok((image, req_align, buf_align, disk_sectors, serial_num, direct, aio)) => {
                self.disk_sectors = disk_sectors;
                self.disk_image = image;
                self.req_align = req_align;
                self.buf_align = buf_align;
                self.serial_num = serial_num;
                self.direct = direct;
                aio_engine = aio;
            }
            Err(e) => {
                error!("Failed to receive config in updating handler {:?}", e);
                self.disk_sectors = 0;
                self.disk_image = None;
                self.req_align = 1;
                self.buf_align = 1;
                self.serial_num = None;
                self.direct = true;
                aio_engine = AioEngine::Native;
            }
        };

        if self.aio.get_engine() != aio_engine {
            match Aio::new(Arc::new(Self::complete_func), aio_engine) {
                Ok(aio) => {
                    self.aio = Box::new(aio);
                }
                Err(e) => {
                    error!("{:?}", e);
                    report_virtio_error(
                        self.interrupt_cb.clone(),
                        self.driver_features,
                        &self.device_broken,
                    );
                    return;
                }
            }
        }

        if let Err(e) = (self.interrupt_cb)(&VirtioInterruptType::Config, None, false) {
            error!(
                "{:?}. {:?}",
                VirtioError::InterruptTrigger("block", VirtioInterruptType::Config),
                e
            );
            report_virtio_error(
                self.interrupt_cb.clone(),
                self.driver_features,
                &self.device_broken,
            );
            return;
        }

        if let Err(ref e) = self.process_queue() {
            error!("Failed to handle block IO for updating handler {:?}", e);
        }
    }
}

fn build_event_notifier(
    fd: RawFd,
    handlers: Vec<Rc<NotifierCallback>>,
    handler_poll: Option<Box<NotifierCallback>>,
) -> EventNotifier {
    let mut notifier = EventNotifier::new(
        NotifierOperation::AddShared,
        fd,
        None,
        EventSet::IN,
        handlers,
    );
    notifier.handler_poll = handler_poll;
    notifier
}

impl EventNotifierHelper for BlockIoHandler {
    fn internal_notifiers(handler: Arc<Mutex<Self>>) -> Vec<EventNotifier> {
        let handler_raw = handler.lock().unwrap();
        let mut notifiers = Vec::new();

        // Register event notifier for update_evt.
        let h_clone = handler.clone();
        let h: Rc<NotifierCallback> = Rc::new(move |_, fd: RawFd| {
            read_fd(fd);
            let mut h_lock = h_clone.lock().unwrap();
            if h_lock.device_broken.load(Ordering::SeqCst) {
                return None;
            }
            h_lock.update_evt_handler();
            None
        });
        notifiers.push(build_event_notifier(
            handler_raw.update_evt.as_raw_fd(),
            vec![h],
            None,
        ));

        // Register event notifier for queue_evt.
        let h_clone = handler.clone();
        let h: Rc<NotifierCallback> = Rc::new(move |_, fd: RawFd| {
            read_fd(fd);
            let mut h_lock = h_clone.lock().unwrap();
            if h_lock.device_broken.load(Ordering::SeqCst) {
                return None;
            }
            if let Err(ref e) = h_lock.process_queue() {
                error!("Failed to handle block IO {:?}", e);
            }
            None
        });
        let h_clone = handler.clone();
        let handler_iopoll: Box<NotifierCallback> = Box::new(move |_, _fd: RawFd| {
            let mut h_lock = h_clone.lock().unwrap();
            if h_lock.device_broken.load(Ordering::SeqCst) {
                return None;
            }
            match h_lock.process_queue() {
                Ok(done) => {
                    if done {
                        Some(Vec::new())
                    } else {
                        None
                    }
                }
                Err(e) => {
                    error!("Failed to handle block IO {:?}", e);
                    None
                }
            }
        });
        notifiers.push(build_event_notifier(
            handler_raw.queue_evt.as_raw_fd(),
            vec![h],
            Some(handler_iopoll),
        ));

        // Register timer event notifier for IO limits
        if let Some(lb) = handler_raw.leak_bucket.as_ref() {
            let h_clone = handler.clone();
            let h: Rc<NotifierCallback> = Rc::new(move |_, fd: RawFd| {
                read_fd(fd);
                let mut h_lock = h_clone.lock().unwrap();
                if h_lock.device_broken.load(Ordering::SeqCst) {
                    return None;
                }
                if let Some(lb) = h_lock.leak_bucket.as_mut() {
                    lb.clear_timer();
                }
                if let Err(ref e) = h_lock.process_queue() {
                    error!("Failed to handle block IO {:?}", e);
                }
                None
            });
            notifiers.push(build_event_notifier(lb.as_raw_fd(), vec![h], None));
        }

        // Register event notifier for aio.
        let h_clone = handler.clone();
        let h: Rc<NotifierCallback> = Rc::new(move |_, fd: RawFd| {
            read_fd(fd);
            let mut h_lock = h_clone.lock().unwrap();
            if h_lock.device_broken.load(Ordering::SeqCst) {
                return None;
            }
            if let Err(ref e) = h_lock.aio_complete_handler() {
                error!("Failed to handle aio {:?}", e);
            }
            None
        });
        let h_clone = handler.clone();
        let handler_iopoll: Box<NotifierCallback> = Box::new(move |_, _fd: RawFd| {
            let mut h_lock = h_clone.lock().unwrap();
            if h_lock.device_broken.load(Ordering::SeqCst) {
                return None;
            }
            if h_lock.aio.get_engine() == AioEngine::Off {
                return None;
            }
            match h_lock.aio_complete_handler() {
                Ok(done) => {
                    if done {
                        Some(Vec::new())
                    } else {
                        None
                    }
                }
                Err(e) => {
                    error!("Failed to handle aio {:?}", e);
                    None
                }
            }
        });
        notifiers.push(build_event_notifier(
            handler_raw.aio.fd.as_raw_fd(),
            vec![h],
            Some(handler_iopoll),
        ));

        notifiers
    }
}

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, Default)]
struct VirtioBlkGeometry {
    cylinders: u16,
    heads: u8,
    sectors: u8,
}

impl ByteCode for VirtioBlkGeometry {}

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, Default)]
pub struct VirtioBlkConfig {
    /// The capacity in 512 byte sectors.
    capacity: u64,
    /// The maximum segment size.
    size_max: u32,
    /// Tne maximum number of segments.
    pub seg_max: u32,
    /// Geometry of the block device.
    geometry: VirtioBlkGeometry,
    /// Block size of device.
    blk_size: u32,
    /// Exponent for physical block per logical block.
    physical_block_exp: u8,
    /// Alignment offset in logical blocks.
    alignment_offset: u8,
    /// Minimum I/O size without performance penalty in logical blocks.
    min_io_size: u16,
    /// Optimal sustained I/O size in logical blocks.
    opt_io_size: u32,
    /// Writeback mode.
    wce: u8,
    /// Reserved data.
    unused: u8,
    /// Number of virtio queues, only available when `VIRTIO_BLK_F_MQ` is set.
    pub num_queues: u16,
    /// The maximum discard sectors for one segment.
    pub max_discard_sectors: u32,
    /// The maximum number of discard segments in a discard command.
    pub max_discard_seg: u32,
    /// Discard commands must be aligned to this number of sectors.
    pub discard_sector_alignment: u32,
    /// The maximum number of write zeros sectors.
    pub max_write_zeroes_sectors: u32,
    /// The maximum number of segments in a write zeroes command.
    pub max_write_zeroes_seg: u32,
    /// Deallocation of one or more of the sectors.
    pub write_zeroes_may_unmap: u8,
    /// Reserved data.
    unused1: [u8; 3],
}

impl ByteCode for VirtioBlkConfig {}

/// State of block device.
#[repr(C)]
#[derive(Clone, Copy, Desc, ByteCode)]
#[desc_version(compat_version = "0.1.0")]
pub struct BlockState {
    /// Bitmask of features supported by the backend.
    pub device_features: u64,
    /// Bit mask of features negotiated by the backend and the frontend.
    pub driver_features: u64,
    /// Config space of the block device.
    pub config_space: VirtioBlkConfig,
    /// Device broken status.
    broken: bool,
}

/// Block device structure.
pub struct Block {
    /// Configuration of the block device.
    blk_cfg: BlkDevConfig,
    /// Image file opened.
    disk_image: Option<Arc<File>>,
    /// The align requirement of request(offset/len).
    pub req_align: u32,
    /// The align requirement of buffer(iova_base).
    pub buf_align: u32,
    /// Number of sectors of the image file.
    disk_sectors: u64,
    /// Status of block device.
    state: BlockState,
    /// Callback to trigger interrupt.
    interrupt_cb: Option<Arc<VirtioInterrupt>>,
    /// The sending half of Rust's channel to send the image file.
    senders: Vec<Sender<SenderConfig>>,
    /// Eventfd for config space update.
    update_evts: Vec<Arc<EventFd>>,
    /// Eventfd for device deactivate.
    deactivate_evts: Vec<RawFd>,
    /// Device is broken or not.
    broken: Arc<AtomicBool>,
    /// Drive backend files.
    drive_files: Arc<Mutex<HashMap<String, DriveFile>>>,
}

impl Block {
    pub fn new(
        blk_cfg: BlkDevConfig,
        drive_files: Arc<Mutex<HashMap<String, DriveFile>>>,
    ) -> Block {
        Self {
            blk_cfg,
            disk_image: None,
            req_align: 1,
            buf_align: 1,
            disk_sectors: 0,
            state: BlockState::default(),
            interrupt_cb: None,
            senders: Vec::new(),
            update_evts: Vec::new(),
            deactivate_evts: Vec::new(),
            broken: Arc::new(AtomicBool::new(false)),
            drive_files,
        }
    }

    fn build_device_config_space(&mut self) {
        // capacity: 64bits
        let num_sectors = DUMMY_IMG_SIZE >> SECTOR_SHIFT;
        self.state.config_space.capacity = num_sectors;
        // seg_max = queue_size - 2: 32bits
        self.state.config_space.seg_max = self.queue_size() as u32 - 2;

        if self.blk_cfg.discard {
            self.state.device_features |= 1_u64 << VIRTIO_BLK_F_DISCARD;
            // Just support one segment per request.
            self.state.config_space.max_discard_seg = 1;
            // The default discard alignment is 1 sector.
            self.state.config_space.discard_sector_alignment = 1;
            self.state.config_space.max_discard_sectors = MAX_REQUEST_SECTORS;
        }

        if self.blk_cfg.write_zeroes != WriteZeroesState::Off {
            self.state.device_features |= 1_u64 << VIRTIO_BLK_F_WRITE_ZEROES;
            // Just support one segment per request.
            self.state.config_space.max_write_zeroes_seg = 1;
            self.state.config_space.max_write_zeroes_sectors = MAX_REQUEST_SECTORS;
            self.state.config_space.write_zeroes_may_unmap = 1;
        }
    }

    fn get_blk_config_size(&self) -> u64 {
        if virtio_has_feature(self.state.device_features, VIRTIO_BLK_F_WRITE_ZEROES) {
            offset_of!(VirtioBlkConfig, unused1) as u64
        } else if virtio_has_feature(self.state.device_features, VIRTIO_BLK_F_DISCARD) {
            offset_of!(VirtioBlkConfig, max_write_zeroes_sectors) as u64
        } else {
            offset_of!(VirtioBlkConfig, max_discard_sectors) as u64
        }
    }
}

impl VirtioDevice for Block {
    /// Realize virtio block device.
    fn realize(&mut self) -> Result<()> {
        // if iothread not found, return err
        if self.blk_cfg.iothread.is_some()
            && EventLoop::get_ctx(self.blk_cfg.iothread.as_ref()).is_none()
        {
            bail!(
                "IOThread {:?} of Block is not configured in params.",
                self.blk_cfg.iothread,
            );
        }

        self.state.device_features = (1_u64 << VIRTIO_F_VERSION_1) | (1_u64 << VIRTIO_BLK_F_FLUSH);
        if self.blk_cfg.read_only {
            self.state.device_features |= 1_u64 << VIRTIO_BLK_F_RO;
        };
        self.state.device_features |= 1_u64 << VIRTIO_F_RING_INDIRECT_DESC;
        self.state.device_features |= 1_u64 << VIRTIO_BLK_F_SEG_MAX;
        self.state.device_features |= 1_u64 << VIRTIO_F_RING_EVENT_IDX;

        self.build_device_config_space();

        if self.blk_cfg.queues > 1 {
            self.state.device_features |= 1_u64 << VIRTIO_BLK_F_MQ;
            self.state.config_space.num_queues = self.blk_cfg.queues;
        }

        self.disk_image = None;
        self.disk_sectors = DUMMY_IMG_SIZE >> SECTOR_SHIFT;
        self.req_align = 1;
        self.buf_align = 1;
        if !self.blk_cfg.path_on_host.is_empty() {
            let drive_files = self.drive_files.lock().unwrap();
            let mut file = VmConfig::fetch_drive_file(&drive_files, &self.blk_cfg.path_on_host)?;
            let alignments = VmConfig::fetch_drive_align(&drive_files, &self.blk_cfg.path_on_host)?;
            let disk_size = file
                .seek(SeekFrom::End(0))
                .with_context(|| "Failed to seek the end for block")?;

            self.disk_image = Some(Arc::new(file));
            self.disk_sectors = disk_size >> SECTOR_SHIFT;
            self.req_align = alignments.0;
            self.buf_align = alignments.1;
        }
        self.state.config_space.capacity = self.disk_sectors;

        Ok(())
    }

    fn unrealize(&mut self) -> Result<()> {
        MigrationManager::unregister_device_instance(BlockState::descriptor(), &self.blk_cfg.id);
        Ok(())
    }

    /// Get the virtio device type, refer to Virtio Spec.
    fn device_type(&self) -> u32 {
        VIRTIO_TYPE_BLOCK
    }

    /// Get the count of virtio device queues.
    fn queue_num(&self) -> usize {
        self.blk_cfg.queues as usize
    }

    /// Get the queue size of virtio device.
    fn queue_size(&self) -> u16 {
        self.blk_cfg.queue_size
    }

    /// Get device features from host.
    fn get_device_features(&self, features_select: u32) -> u32 {
        read_u32(self.state.device_features, features_select)
    }

    /// Set driver features by guest.
    fn set_driver_features(&mut self, page: u32, value: u32) {
        self.state.driver_features = self.checked_driver_features(page, value);
    }

    /// Get driver features by guest.
    fn get_driver_features(&self, features_select: u32) -> u32 {
        read_u32(self.state.driver_features, features_select)
    }

    /// Read data of config from guest.
    fn read_config(&self, offset: u64, mut data: &mut [u8]) -> Result<()> {
        let config_len = self.get_blk_config_size();
        let read_end = offset as usize + data.len();
        if offset
            .checked_add(data.len() as u64)
            .filter(|&end| end <= config_len)
            .is_none()
        {
            return Err(anyhow!(VirtioError::DevConfigOverflow(offset, config_len)));
        }

        let config_slice = self.state.config_space.as_bytes();
        data.write_all(&config_slice[(offset as usize)..read_end])?;

        Ok(())
    }

    /// Write data to config from guest.
    fn write_config(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let config_len = self.get_blk_config_size();
        if offset
            .checked_add(data.len() as u64)
            .filter(|&end| end <= config_len)
            .is_none()
        {
            return Err(anyhow!(VirtioError::DevConfigOverflow(offset, config_len)));
        }
        // The only writable field is "writeback", but it's not supported for now,
        // so do nothing here.

        Ok(())
    }

    /// Activate the virtio device, this function is called by vcpu thread when frontend
    /// virtio driver is ready and write `DRIVER_OK` to backend.
    fn activate(
        &mut self,
        mem_space: Arc<AddressSpace>,
        interrupt_cb: Arc<VirtioInterrupt>,
        queues: &[Arc<Mutex<Queue>>],
        queue_evts: Vec<Arc<EventFd>>,
    ) -> Result<()> {
        self.interrupt_cb = Some(interrupt_cb.clone());
        for (index, queue) in queues.iter().enumerate() {
            if !queue.lock().unwrap().is_enabled() {
                continue;
            }
            let (sender, receiver) = channel();
            let update_evt = Arc::new(EventFd::new(libc::EFD_NONBLOCK)?);
            let aio = Box::new(Aio::new(
                Arc::new(BlockIoHandler::complete_func),
                self.blk_cfg.aio,
            )?);
            let driver_features = self.state.driver_features;
            let handler = BlockIoHandler {
                queue: queue.clone(),
                queue_evt: queue_evts[index].clone(),
                mem_space: mem_space.clone(),
                disk_image: self.disk_image.clone(),
                req_align: self.req_align,
                buf_align: self.buf_align,
                disk_sectors: self.disk_sectors,
                direct: self.blk_cfg.direct,
                serial_num: self.blk_cfg.serial_num.clone(),
                aio,
                driver_features,
                receiver,
                update_evt: update_evt.clone(),
                device_broken: self.broken.clone(),
                interrupt_cb: interrupt_cb.clone(),
                iothread: self.blk_cfg.iothread.clone(),
                leak_bucket: match self.blk_cfg.iops {
                    Some(iops) => Some(LeakBucket::new(iops)?),
                    None => None,
                },
                discard: self.blk_cfg.discard,
                write_zeroes: self.blk_cfg.write_zeroes,
            };

            let notifiers = EventNotifierHelper::internal_notifiers(Arc::new(Mutex::new(handler)));
            register_event_helper(
                notifiers,
                self.blk_cfg.iothread.as_ref(),
                &mut self.deactivate_evts,
            )?;
            self.update_evts.push(update_evt);
            self.senders.push(sender);
        }
        self.broken.store(false, Ordering::SeqCst);

        Ok(())
    }

    fn deactivate(&mut self) -> Result<()> {
        unregister_event_helper(self.blk_cfg.iothread.as_ref(), &mut self.deactivate_evts)?;
        self.update_evts.clear();
        self.senders.clear();
        Ok(())
    }

    fn update_config(&mut self, dev_config: Option<Arc<dyn ConfigCheck>>) -> Result<()> {
        if let Some(conf) = dev_config {
            self.blk_cfg = conf
                .as_any()
                .downcast_ref::<BlkDevConfig>()
                .unwrap()
                .clone();
            // microvm type block device don't support multiple queue.
            self.blk_cfg.queues = QUEUE_NUM_BLK as u16;
        } else {
            self.blk_cfg = Default::default();
        }

        self.realize()?;

        for sender in &self.senders {
            sender
                .send((
                    self.disk_image.clone(),
                    self.req_align,
                    self.buf_align,
                    self.disk_sectors,
                    self.blk_cfg.serial_num.clone(),
                    self.blk_cfg.direct,
                    self.blk_cfg.aio,
                ))
                .with_context(|| VirtioError::ChannelSend("image fd".to_string()))?;
        }
        for update_evt in &self.update_evts {
            update_evt
                .write(1)
                .with_context(|| VirtioError::EventFdWrite)?;
        }

        Ok(())
    }
}

// SAFETY: Send and Sync is not auto-implemented for `Sender` type.
// Implementing them is safe because `Sender` field of Block won't
// change in migration workflow.
unsafe impl Sync for Block {}

impl StateTransfer for Block {
    fn get_state_vec(&self) -> migration::Result<Vec<u8>> {
        let mut state = self.state;
        state.broken = self.broken.load(Ordering::SeqCst);
        Ok(state.as_bytes().to_vec())
    }

    fn set_state_mut(&mut self, state: &[u8]) -> migration::Result<()> {
        self.state = *BlockState::from_bytes(state)
            .with_context(|| migration::error::MigrationError::FromBytesError("BLOCK"))?;
        self.broken.store(self.state.broken, Ordering::SeqCst);
        Ok(())
    }

    fn get_device_alias(&self) -> u64 {
        MigrationManager::get_desc_alias(&BlockState::descriptor().name).unwrap_or(!0)
    }
}

impl MigrationHook for Block {}

impl VirtioTrace for BlockIoHandler {}
impl VirtioTrace for AioCompleteCb {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    use address_space::{AddressSpace, GuestAddress, HostMemMapping, Region};
    use machine_manager::config::{IothreadConfig, VmConfig, DEFAULT_VIRTQUEUE_SIZE};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::{thread, time::Duration};
    use vmm_sys_util::tempfile::TempFile;

    const QUEUE_NUM_BLK: usize = 1;
    const CONFIG_SPACE_SIZE: usize = 60;
    const VIRTQ_DESC_F_NEXT: u16 = 0x01;
    const VIRTQ_DESC_F_WRITE: u16 = 0x02;
    const SYSTEM_SPACE_SIZE: u64 = (1024 * 1024) as u64;

    impl Default for Block {
        fn default() -> Self {
            Block {
                blk_cfg: Default::default(),
                disk_image: None,
                req_align: 1,
                buf_align: 1,
                disk_sectors: 0,
                state: BlockState::default(),
                interrupt_cb: None,
                senders: Vec::new(),
                update_evts: Vec::new(),
                deactivate_evts: Vec::new(),
                broken: Arc::new(AtomicBool::new(false)),
                drive_files: Arc::new(Mutex::new(HashMap::new())),
            }
        }
    }

    // build dummy address space of vm
    fn address_space_init() -> Arc<AddressSpace> {
        let root = Region::init_container_region(1 << 36);
        let sys_space = AddressSpace::new(root).unwrap();
        let host_mmap = Arc::new(
            HostMemMapping::new(
                GuestAddress(0),
                None,
                SYSTEM_SPACE_SIZE,
                None,
                false,
                false,
                false,
            )
            .unwrap(),
        );
        sys_space
            .root()
            .add_subregion(
                Region::init_ram_region(host_mmap.clone()),
                host_mmap.start_address().raw_value(),
            )
            .unwrap();
        sys_space
    }

    // Use different input parameters to verify block `new()` and `realize()` functionality.
    #[test]
    fn test_block_init() {
        // New block device
        let mut block = Block::default();
        assert_eq!(block.disk_sectors, 0);
        assert_eq!(block.state.device_features, 0);
        assert_eq!(block.state.driver_features, 0);
        assert_eq!(block.state.config_space.as_bytes().len(), CONFIG_SPACE_SIZE);
        assert!(block.disk_image.is_none());
        assert!(block.interrupt_cb.is_none());
        assert!(block.senders.is_empty());

        // Realize block device: create TempFile as backing file.
        block.blk_cfg.read_only = true;
        block.blk_cfg.direct = false;
        let f = TempFile::new().unwrap();
        block.blk_cfg.path_on_host = f.as_path().to_str().unwrap().to_string();
        VmConfig::add_drive_file(
            &mut block.drive_files.lock().unwrap(),
            &block.blk_cfg.path_on_host,
            block.blk_cfg.read_only,
            block.blk_cfg.direct,
        )
        .unwrap();
        assert!(block.realize().is_ok());

        assert_eq!(block.device_type(), VIRTIO_TYPE_BLOCK);
        assert_eq!(block.queue_num(), QUEUE_NUM_BLK);
        assert_eq!(block.queue_size(), DEFAULT_VIRTQUEUE_SIZE);
    }

    // Test `write_config` and `read_config`. The main contests include: compare expect data and
    // read data are not same; Input invalid offset or data length, it will failed.
    #[test]
    fn test_read_write_config() {
        let mut block = Block::default();
        block.realize().unwrap();

        let expect_config_space: [u8; 8] = [0x00, 020, 0x00, 0x00, 0x00, 0x00, 0x50, 0x00];
        let mut read_config_space = [0u8; 8];
        block.write_config(0, &expect_config_space).unwrap();
        block.read_config(0, &mut read_config_space).unwrap();
        assert_ne!(read_config_space, expect_config_space);

        // Invalid write
        assert!(block
            .write_config(CONFIG_SPACE_SIZE as u64 + 1, &expect_config_space)
            .is_err());
        let errlen_config_space = [0u8; CONFIG_SPACE_SIZE + 1];
        assert!(block.write_config(0, &errlen_config_space).is_err());
        // Invalid read
        read_config_space = expect_config_space;
        assert!(block
            .read_config(CONFIG_SPACE_SIZE as u64 + 1, &mut read_config_space)
            .is_err());
    }

    // Test `get_device_features` and `set_driver_features`. The main contests include: If the
    // device feature is 0, all driver features are not supported; If both the device feature bit
    // and the front-end driver feature bit are supported at the same time,  this driver feature
    // bit is supported.
    #[test]
    fn test_block_features() {
        let mut block = Block::default();

        // If the device feature is 0, all driver features are not supported.
        block.state.device_features = 0;
        let driver_feature: u32 = 0xFF;
        let page = 0_u32;
        block.set_driver_features(page, driver_feature);
        assert_eq!(block.state.driver_features, 0_u64);
        assert_eq!(block.get_driver_features(page) as u64, 0_u64);
        assert_eq!(block.get_device_features(0_u32), 0_u32);

        let driver_feature: u32 = 0xFF;
        let page = 1_u32;
        block.set_driver_features(page, driver_feature);
        assert_eq!(block.state.driver_features, 0_u64);
        assert_eq!(block.get_driver_features(page) as u64, 0_u64);
        assert_eq!(block.get_device_features(1_u32), 0_u32);

        // If both the device feature bit and the front-end driver feature bit are
        // supported at the same time,  this driver feature bit is supported.
        block.state.device_features =
            1_u64 << VIRTIO_F_VERSION_1 | 1_u64 << VIRTIO_F_RING_INDIRECT_DESC;
        let driver_feature: u32 = (1_u64 << VIRTIO_F_RING_INDIRECT_DESC) as u32;
        let page = 0_u32;
        block.set_driver_features(page, driver_feature);
        assert_eq!(
            block.state.driver_features,
            (1_u64 << VIRTIO_F_RING_INDIRECT_DESC)
        );
        assert_eq!(
            block.get_driver_features(page) as u64,
            (1_u64 << VIRTIO_F_RING_INDIRECT_DESC)
        );
        assert_eq!(
            block.get_device_features(page),
            (1_u32 << VIRTIO_F_RING_INDIRECT_DESC)
        );
        block.state.driver_features = 0;

        block.state.device_features = 1_u64 << VIRTIO_F_VERSION_1;
        let driver_feature: u32 = (1_u64 << VIRTIO_F_RING_INDIRECT_DESC) as u32;
        let page = 0_u32;
        block.set_driver_features(page, driver_feature);
        assert_eq!(block.state.driver_features, 0);
        assert_eq!(block.get_driver_features(page), 0);
        assert_eq!(block.get_device_features(page), 0_u32);
        block.state.driver_features = 0;
    }

    // Test `get_serial_num_config`. The function will output the shorter length between 20
    // with serial_num length.
    #[test]
    fn test_serial_num_config() {
        let serial_num = "fldXlNNdCeqMvoIfEFogBxlL";
        let serial_num_arr = serial_num.as_bytes();
        let id_bytes = get_serial_num_config(&serial_num);
        assert_eq!(id_bytes[..], serial_num_arr[..20]);
        assert_eq!(id_bytes.len(), 20);

        let serial_num = "7681194149";
        let serial_num_arr = serial_num.as_bytes();
        let id_bytes = get_serial_num_config(&serial_num);
        assert_eq!(id_bytes[..10], serial_num_arr[..]);
        assert_eq!(id_bytes.len(), 20);

        let serial_num = "";
        let id_bytes_temp = get_serial_num_config(&serial_num);
        assert_eq!(id_bytes_temp[..], [0; 20]);
        assert_eq!(id_bytes_temp.len(), 20);
    }

    // Test iothread and qos capability. The function will spawn a thread called 'iothread', then
    // io request will be handled by this thread.
    #[test]
    fn test_iothread() {
        let thread_name = "io1".to_string();

        // spawn io thread
        let io_conf = IothreadConfig {
            id: thread_name.clone(),
        };
        EventLoop::object_init(&Some(vec![io_conf])).unwrap();

        let mut block = Block::default();
        let file = TempFile::new().unwrap();
        block.blk_cfg.path_on_host = file.as_path().to_str().unwrap().to_string();
        block.blk_cfg.direct = false;

        // config iothread and iops
        block.blk_cfg.iothread = Some(thread_name);
        block.blk_cfg.iops = Some(100);

        VmConfig::add_drive_file(
            &mut block.drive_files.lock().unwrap(),
            &block.blk_cfg.path_on_host,
            block.blk_cfg.read_only,
            block.blk_cfg.direct,
        )
        .unwrap();

        let mem_space = address_space_init();
        let interrupt_evt = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let interrupt_status = Arc::new(AtomicU32::new(0));
        let interrupt_cb = Arc::new(Box::new(
            move |int_type: &VirtioInterruptType, _queue: Option<&Queue>, _needs_reset: bool| {
                let status = match int_type {
                    VirtioInterruptType::Config => VIRTIO_MMIO_INT_CONFIG,
                    VirtioInterruptType::Vring => VIRTIO_MMIO_INT_VRING,
                };
                interrupt_status.fetch_or(status as u32, Ordering::SeqCst);
                interrupt_evt
                    .write(1)
                    .with_context(|| VirtioError::EventFdWrite)?;

                Ok(())
            },
        ) as VirtioInterrupt);

        let mut queue_config = QueueConfig::new(DEFAULT_VIRTQUEUE_SIZE);
        queue_config.desc_table = GuestAddress(0);
        queue_config.addr_cache.desc_table_host =
            mem_space.get_host_address(queue_config.desc_table).unwrap();
        queue_config.avail_ring = GuestAddress(16 * DEFAULT_VIRTQUEUE_SIZE as u64);
        queue_config.addr_cache.avail_ring_host =
            mem_space.get_host_address(queue_config.avail_ring).unwrap();
        queue_config.used_ring = GuestAddress(32 * DEFAULT_VIRTQUEUE_SIZE as u64);
        queue_config.addr_cache.used_ring_host =
            mem_space.get_host_address(queue_config.used_ring).unwrap();
        queue_config.size = DEFAULT_VIRTQUEUE_SIZE;
        queue_config.ready = true;

        let queues: Vec<Arc<Mutex<Queue>>> =
            vec![Arc::new(Mutex::new(Queue::new(queue_config, 1).unwrap()))];
        let event = Arc::new(EventFd::new(libc::EFD_NONBLOCK).unwrap());

        // activate block device
        block
            .activate(
                mem_space.clone(),
                interrupt_cb,
                &queues,
                vec![event.clone()],
            )
            .unwrap();

        // make first descriptor entry
        let desc = SplitVringDesc {
            addr: GuestAddress(0x100),
            len: 16,
            flags: VIRTQ_DESC_F_NEXT,
            next: 1,
        };
        mem_space
            .write_object::<SplitVringDesc>(&desc, GuestAddress(queue_config.desc_table.0))
            .unwrap();

        // write RequestOutHeader to first desc
        let req_head = RequestOutHeader {
            request_type: 0, // read
            io_prio: 0,
            sector: 0,
        };
        mem_space
            .write_object::<RequestOutHeader>(&req_head, GuestAddress(0x100))
            .unwrap();

        // making the second descriptor entry to receive data from device
        let desc = SplitVringDesc {
            addr: GuestAddress(0x200),
            len: 16,
            flags: VIRTQ_DESC_F_WRITE,
            next: 2,
        };
        mem_space
            .write_object::<SplitVringDesc>(
                &desc,
                GuestAddress(queue_config.desc_table.0 + 16 as u64),
            )
            .unwrap();

        // write avail_ring idx
        mem_space
            .write_object::<u16>(&0, GuestAddress(queue_config.avail_ring.0 + 4 as u64))
            .unwrap();

        // write avail_ring id
        mem_space
            .write_object::<u16>(&1, GuestAddress(queue_config.avail_ring.0 + 2 as u64))
            .unwrap();

        // imitating guest OS to send notification.
        event.write(1).unwrap();

        // waiting for io handled
        let mut wait = 10; // wait for 2 seconds
        loop {
            thread::sleep(Duration::from_millis(200));

            wait -= 1;
            if wait == 0 {
                assert_eq!(0, 1); // timeout failed
            }

            // get used_ring data
            let idx = mem_space
                .read_object::<u16>(GuestAddress(queue_config.used_ring.0 + 2 as u64))
                .unwrap();
            if idx == 1 {
                break;
            }
        }
    }
}
