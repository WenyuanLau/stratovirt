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

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::VirtioError;
use address_space::{AddressRange, AddressSpace, GuestAddress, RegionIoEventFd};
use byteorder::{ByteOrder, LittleEndian};
use log::{error, warn};
#[cfg(target_arch = "x86_64")]
use machine_manager::config::{BootSource, Param};
use migration::{DeviceStateDesc, FieldDesc, MigrationHook, MigrationManager, StateTransfer};
use migration_derive::{ByteCode, Desc};
use sysbus::{SysBus, SysBusDevOps, SysBusDevType, SysRes};
use util::byte_code::ByteCode;
use vmm_sys_util::eventfd::EventFd;

use crate::{
    virtio_has_feature, Queue, QueueConfig, VirtioDevice, VirtioInterrupt, VirtioInterruptType,
    CONFIG_STATUS_ACKNOWLEDGE, CONFIG_STATUS_DRIVER, CONFIG_STATUS_DRIVER_OK, CONFIG_STATUS_FAILED,
    CONFIG_STATUS_FEATURES_OK, CONFIG_STATUS_NEEDS_RESET, NOTIFY_REG_OFFSET,
    QUEUE_TYPE_PACKED_VRING, QUEUE_TYPE_SPLIT_VRING, VIRTIO_F_RING_PACKED, VIRTIO_MMIO_INT_CONFIG,
    VIRTIO_MMIO_INT_VRING,
};
use anyhow::{anyhow, bail, Context, Result};

/// Registers of virtio-mmio device refer to Virtio Spec.
/// Magic value - Read Only.
const MAGIC_VALUE_REG: u64 = 0x00;
/// Virtio device version - Read Only.
const VERSION_REG: u64 = 0x04;
/// Virtio device ID - Read Only.
const DEVICE_ID_REG: u64 = 0x08;
/// Virtio vendor ID - Read Only.
const VENDOR_ID_REG: u64 = 0x0c;
/// Bitmask of the features supported by the device(host) (32 bits per set) - Read Only.
const DEVICE_FEATURES_REG: u64 = 0x10;
/// Device (host) features set selector - Write Only.
const DEVICE_FEATURES_SEL_REG: u64 = 0x14;
/// Bitmask of features activated by the driver (guest) (32 bits per set) - Write Only.
const DRIVER_FEATURES_REG: u64 = 0x20;
/// Activated features set selector - Write Only.
const DRIVER_FEATURES_SEL_REG: u64 = 0x24;
/// Queue selector - Write Only.
const QUEUE_SEL_REG: u64 = 0x30;
/// Maximum size of the currently selected queue - Read Only.
const QUEUE_NUM_MAX_REG: u64 = 0x34;
/// Queue size for the currently selected queue - Write Only.
const QUEUE_NUM_REG: u64 = 0x38;
/// Ready bit for the currently selected queue - Read Write.
const QUEUE_READY_REG: u64 = 0x44;
/// Interrupt status - Read Only.
const INTERRUPT_STATUS_REG: u64 = 0x60;
/// Interrupt acknowledge - Write Only.
const INTERRUPT_ACK_REG: u64 = 0x64;
/// Device status register - Read Write.
const STATUS_REG: u64 = 0x70;
/// The low 32bit of queue's Descriptor Table address.
const QUEUE_DESC_LOW_REG: u64 = 0x80;
/// The high 32bit of queue's Descriptor Table address.
const QUEUE_DESC_HIGH_REG: u64 = 0x84;
/// The low 32 bit of queue's Available Ring address.
const QUEUE_AVAIL_LOW_REG: u64 = 0x90;
/// The high 32 bit of queue's Available Ring address.
const QUEUE_AVAIL_HIGH_REG: u64 = 0x94;
/// The low 32bit of queue's Used Ring address.
const QUEUE_USED_LOW_REG: u64 = 0xa0;
/// The high 32bit of queue's Used Ring address.
const QUEUE_USED_HIGH_REG: u64 = 0xa4;
/// Configuration atomicity value.
const CONFIG_GENERATION_REG: u64 = 0xfc;

const VENDOR_ID: u32 = 0;
const MMIO_MAGIC_VALUE: u32 = 0x7472_6976;
const MMIO_VERSION: u32 = 2;

/// The maximum of virtio queue within a virtio device.
const MAXIMUM_NR_QUEUES: usize = 8;

/// HostNotifyInfo includes the info needed for notifying backend from guest.
pub struct HostNotifyInfo {
    /// Eventfds which notify backend to use the avail ring.
    events: Vec<Arc<EventFd>>,
}

impl HostNotifyInfo {
    pub fn new(queue_num: usize) -> Self {
        let mut events = Vec::new();
        for _i in 0..queue_num {
            events.push(Arc::new(EventFd::new(libc::EFD_NONBLOCK).unwrap()));
        }

        HostNotifyInfo { events }
    }
}

/// The state of virtio-mmio device.
#[repr(C)]
#[derive(Copy, Clone, Desc, ByteCode)]
#[desc_version(compat_version = "0.1.0")]
pub struct VirtioMmioState {
    /// Identify if this device is activated by frontend driver.
    activated: bool,
    /// Config space of virtio mmio device.
    config_space: VirtioMmioCommonConfig,
}

/// The configuration of virtio-mmio device, the fields refer to Virtio Spec.
#[derive(Copy, Clone, Default)]
pub struct VirtioMmioCommonConfig {
    /// Bitmask of the features supported by the device (host)(32 bits per set).
    features_select: u32,
    /// Device (host) feature-setting selector.
    acked_features_select: u32,
    /// Interrupt status value.
    interrupt_status: u32,
    /// Device status.
    device_status: u32,
    /// Configuration atomicity value.
    config_generation: u32,
    /// Queue selector.
    queue_select: u32,
    /// The configuration of queues.
    queues_config: [QueueConfig; MAXIMUM_NR_QUEUES],
    /// The number of queues.
    queue_num: usize,
    /// The type of queue, either be split ring or packed ring.
    queue_type: u16,
}

impl VirtioMmioCommonConfig {
    pub fn new(device: &Arc<Mutex<dyn VirtioDevice>>) -> Self {
        let locked_device = device.lock().unwrap();
        let mut queues_config = [QueueConfig::default(); 8];
        let queue_size = locked_device.queue_size();
        let queue_num = locked_device.queue_num();
        for queue_config in queues_config.iter_mut().take(queue_num) {
            *queue_config = QueueConfig::new(queue_size);
        }

        VirtioMmioCommonConfig {
            queues_config,
            queue_num,
            queue_type: QUEUE_TYPE_SPLIT_VRING,
            ..Default::default()
        }
    }

    /// Check whether virtio device status is as expected.
    fn check_device_status(&self, set: u32, clr: u32) -> bool {
        self.device_status & (set | clr) == set
    }

    /// Get the status of virtio device
    fn get_device_status(&self) -> u32 {
        self.device_status
    }

    /// Get mutable QueueConfig structure of virtio device.
    fn get_mut_queue_config(&mut self) -> Result<&mut QueueConfig> {
        if self.check_device_status(
            CONFIG_STATUS_FEATURES_OK,
            CONFIG_STATUS_DRIVER_OK | CONFIG_STATUS_FAILED,
        ) {
            let queue_select = self.queue_select;
            self.queues_config
                .get_mut(queue_select as usize)
                .with_context(|| {
                    format!(
                        "Mmio-reg queue_select {} overflows for mutable queue config",
                        queue_select,
                    )
                })
        } else {
            Err(anyhow!(VirtioError::DevStatErr(self.device_status)))
        }
    }

    /// Get immutable QueueConfig structure of virtio device.
    fn get_queue_config(&self) -> Result<&QueueConfig> {
        let queue_select = self.queue_select;
        self.queues_config
            .get(queue_select as usize)
            .with_context(|| {
                format!(
                    "Mmio-reg queue_select overflows {} for immutable queue config",
                    queue_select,
                )
            })
    }

    /// Read data from the common config of virtio device.
    /// Return the config value in u32.
    /// # Arguments
    ///
    /// * `device` - Virtio device entity.
    /// * `offset` - The offset of common config.
    fn read_common_config(
        &mut self,
        device: &Arc<Mutex<dyn VirtioDevice>>,
        interrupt_status: &Arc<AtomicU32>,
        offset: u64,
    ) -> Result<u32> {
        let value = match offset {
            MAGIC_VALUE_REG => MMIO_MAGIC_VALUE,
            VERSION_REG => MMIO_VERSION,
            DEVICE_ID_REG => device.lock().unwrap().device_type(),
            VENDOR_ID_REG => VENDOR_ID,
            DEVICE_FEATURES_REG => {
                let mut features = device
                    .lock()
                    .unwrap()
                    .get_device_features(self.features_select);
                if self.features_select == 1 {
                    features |= 0x1; // enable support of VirtIO Version 1
                }
                features
            }
            QUEUE_NUM_MAX_REG => self
                .get_queue_config()
                .map(|config| u32::from(config.max_size))?,
            QUEUE_READY_REG => self.get_queue_config().map(|config| config.ready as u32)?,
            INTERRUPT_STATUS_REG => {
                self.interrupt_status = interrupt_status.load(Ordering::SeqCst);
                self.interrupt_status
            }
            STATUS_REG => self.device_status,
            CONFIG_GENERATION_REG => self.config_generation,
            _ => {
                return Err(anyhow!(VirtioError::MmioRegErr(offset)));
            }
        };

        Ok(value)
    }

    /// Write data to the common config of virtio device.
    ///
    /// # Arguments
    ///
    /// * `device` - Virtio device entity.
    /// * `offset` - The offset of common config.
    /// * `value` - The value to write.
    ///
    /// # Errors
    ///
    /// Returns Error if the offset is out of bound.
    fn write_common_config(
        &mut self,
        device: &Arc<Mutex<dyn VirtioDevice>>,
        interrupt_status: &Arc<AtomicU32>,
        offset: u64,
        value: u32,
    ) -> Result<()> {
        match offset {
            DEVICE_FEATURES_SEL_REG => self.features_select = value,
            DRIVER_FEATURES_REG => {
                if self.check_device_status(
                    CONFIG_STATUS_DRIVER,
                    CONFIG_STATUS_FEATURES_OK | CONFIG_STATUS_FAILED,
                ) {
                    device
                        .lock()
                        .unwrap()
                        .set_driver_features(self.acked_features_select, value);
                    if self.acked_features_select == 1
                        && virtio_has_feature(u64::from(value) << 32, VIRTIO_F_RING_PACKED)
                    {
                        self.queue_type = QUEUE_TYPE_PACKED_VRING;
                    }
                } else {
                    return Err(anyhow!(VirtioError::DevStatErr(self.device_status)));
                }
            }
            DRIVER_FEATURES_SEL_REG => self.acked_features_select = value,
            QUEUE_SEL_REG => self.queue_select = value,
            QUEUE_NUM_REG => self
                .get_mut_queue_config()
                .map(|config| config.size = value as u16)?,
            QUEUE_READY_REG => self
                .get_mut_queue_config()
                .map(|config| config.ready = value == 1)?,
            INTERRUPT_ACK_REG => {
                if self.check_device_status(CONFIG_STATUS_DRIVER_OK, 0) {
                    self.interrupt_status = interrupt_status.fetch_and(!value, Ordering::SeqCst);
                }
            }
            STATUS_REG => self.device_status = value,
            QUEUE_DESC_LOW_REG => self.get_mut_queue_config().map(|config| {
                config.desc_table = GuestAddress(config.desc_table.0 | u64::from(value));
            })?,
            QUEUE_DESC_HIGH_REG => self.get_mut_queue_config().map(|config| {
                config.desc_table = GuestAddress(config.desc_table.0 | (u64::from(value) << 32));
            })?,
            QUEUE_AVAIL_LOW_REG => self.get_mut_queue_config().map(|config| {
                config.avail_ring = GuestAddress(config.avail_ring.0 | u64::from(value));
            })?,
            QUEUE_AVAIL_HIGH_REG => self.get_mut_queue_config().map(|config| {
                config.avail_ring = GuestAddress(config.avail_ring.0 | (u64::from(value) << 32));
            })?,
            QUEUE_USED_LOW_REG => self.get_mut_queue_config().map(|config| {
                config.used_ring = GuestAddress(config.used_ring.0 | u64::from(value));
            })?,
            QUEUE_USED_HIGH_REG => self.get_mut_queue_config().map(|config| {
                config.used_ring = GuestAddress(config.used_ring.0 | (u64::from(value) << 32));
            })?,
            _ => {
                return Err(anyhow!(VirtioError::MmioRegErr(offset)));
            }
        };
        Ok(())
    }
}

/// virtio-mmio device structure.
pub struct VirtioMmioDevice {
    // The entity of low level device.
    pub device: Arc<Mutex<dyn VirtioDevice>>,
    // EventFd used to send interrupt to VM
    interrupt_evt: Arc<EventFd>,
    // Interrupt status.
    interrupt_status: Arc<AtomicU32>,
    // HostNotifyInfo used for guest notifier
    host_notify_info: HostNotifyInfo,
    // The state of virtio mmio device.
    state: Arc<Mutex<VirtioMmioState>>,
    // System address space.
    mem_space: Arc<AddressSpace>,
    // Virtio queues.
    queues: Vec<Arc<Mutex<Queue>>>,
    // System Resource of device.
    res: SysRes,
    /// The function for interrupt triggering.
    interrupt_cb: Option<Arc<VirtioInterrupt>>,
}

impl VirtioMmioDevice {
    pub fn new(mem_space: &Arc<AddressSpace>, device: Arc<Mutex<dyn VirtioDevice>>) -> Self {
        let device_clone = device.clone();
        let queue_num = device_clone.lock().unwrap().queue_num();

        VirtioMmioDevice {
            device,
            interrupt_evt: Arc::new(EventFd::new(libc::EFD_NONBLOCK).unwrap()),
            interrupt_status: Arc::new(AtomicU32::new(0)),
            host_notify_info: HostNotifyInfo::new(queue_num),
            state: Arc::new(Mutex::new(VirtioMmioState {
                activated: false,
                config_space: VirtioMmioCommonConfig::new(&device_clone),
            })),
            mem_space: mem_space.clone(),
            queues: Vec::new(),
            res: SysRes::default(),
            interrupt_cb: None,
        }
    }

    pub fn realize(
        mut self,
        sysbus: &mut SysBus,
        region_base: u64,
        region_size: u64,
        #[cfg(target_arch = "x86_64")] bs: &Arc<Mutex<BootSource>>,
    ) -> Result<Arc<Mutex<Self>>> {
        self.assign_interrupt_cb();
        self.device
            .lock()
            .unwrap()
            .realize()
            .with_context(|| "Failed to realize virtio.")?;

        if region_base >= sysbus.mmio_region.1 {
            bail!("Mmio region space exhausted.");
        }
        self.set_sys_resource(sysbus, region_base, region_size)?;
        let dev = Arc::new(Mutex::new(self));
        sysbus.attach_device(&dev, region_base, region_size)?;

        #[cfg(target_arch = "x86_64")]
        bs.lock().unwrap().kernel_cmdline.push(Param {
            param_type: "virtio_mmio.device".to_string(),
            value: format!(
                "{}@0x{:08x}:{}",
                region_size,
                region_base,
                dev.lock().unwrap().res.irq
            ),
        });
        Ok(dev)
    }

    /// Activate the virtio device, this function is called by vcpu thread when frontend
    /// virtio driver is ready and write `DRIVER_OK` to backend.
    fn activate(&mut self) -> Result<()> {
        let mut locked_state = self.state.lock().unwrap();
        let queue_num = locked_state.config_space.queue_num;
        let queue_type = locked_state.config_space.queue_type;
        let queues_config = &mut locked_state.config_space.queues_config[0..queue_num];
        let cloned_mem_space = self.mem_space.clone();
        for q_config in queues_config.iter_mut() {
            q_config.addr_cache.desc_table_host = cloned_mem_space
                .get_host_address(q_config.desc_table)
                .unwrap_or(0);
            q_config.addr_cache.avail_ring_host = cloned_mem_space
                .get_host_address(q_config.avail_ring)
                .unwrap_or(0);
            q_config.addr_cache.used_ring_host = cloned_mem_space
                .get_host_address(q_config.used_ring)
                .unwrap_or(0);
            let queue = Queue::new(*q_config, queue_type)?;
            if !queue.is_valid(&self.mem_space) {
                bail!("Invalid queue");
            }
            self.queues.push(Arc::new(Mutex::new(queue)));
        }
        drop(locked_state);

        let mut queue_evts = Vec::<Arc<EventFd>>::new();
        for fd in self.host_notify_info.events.iter() {
            queue_evts.push(fd.clone());
        }

        let mut events = Vec::new();
        for _i in 0..self.device.lock().unwrap().queue_num() {
            events.push(Arc::new(EventFd::new(libc::EFD_NONBLOCK).unwrap()));
        }

        self.device.lock().unwrap().set_guest_notifiers(&events)?;

        if let Some(cb) = self.interrupt_cb.clone() {
            self.device.lock().unwrap().activate(
                self.mem_space.clone(),
                cb,
                &self.queues,
                queue_evts,
            )?;
        } else {
            bail!("Failed to activate device: No interrupt callback");
        }

        Ok(())
    }

    fn assign_interrupt_cb(&mut self) {
        let interrupt_status = self.interrupt_status.clone();
        let interrupt_evt = self.interrupt_evt.clone();
        let cloned_state = self.state.clone();
        let cb = Arc::new(Box::new(
            move |int_type: &VirtioInterruptType, _queue: Option<&Queue>, needs_reset: bool| {
                let status = match int_type {
                    VirtioInterruptType::Config => {
                        let mut locked_state = cloned_state.lock().unwrap();
                        if needs_reset {
                            locked_state.config_space.device_status |= CONFIG_STATUS_NEEDS_RESET;
                            if locked_state.config_space.device_status & CONFIG_STATUS_DRIVER_OK
                                == 0
                            {
                                return Ok(());
                            }
                        }
                        locked_state.config_space.config_generation += 1;
                        // Use (CONFIG | VRING) instead of CONFIG, it can be used to solve the
                        // IO stuck problem by change the device configure.
                        VIRTIO_MMIO_INT_CONFIG | VIRTIO_MMIO_INT_VRING
                    }
                    VirtioInterruptType::Vring => VIRTIO_MMIO_INT_VRING,
                };
                interrupt_status.fetch_or(status, Ordering::SeqCst);
                interrupt_evt
                    .write(1)
                    .with_context(|| VirtioError::EventFdWrite)?;

                Ok(())
            },
        ) as VirtioInterrupt);

        self.interrupt_cb = Some(cb);
    }
}

impl SysBusDevOps for VirtioMmioDevice {
    /// Read data by virtio driver from VM.
    fn read(&mut self, data: &mut [u8], _base: GuestAddress, offset: u64) -> bool {
        match offset {
            0x00..=0xff if data.len() == 4 => {
                let value = match self.state.lock().unwrap().config_space.read_common_config(
                    &self.device,
                    &self.interrupt_status,
                    offset,
                ) {
                    Ok(v) => v,
                    Err(ref e) => {
                        error!(
                            "Failed to read mmio register {}, type: {}, {:?}",
                            offset,
                            self.device.lock().unwrap().device_type(),
                            e,
                        );
                        return false;
                    }
                };
                LittleEndian::write_u32(data, value);
            }
            0x100..=0xfff => {
                if let Err(ref e) = self
                    .device
                    .lock()
                    .unwrap()
                    .read_config(offset - 0x100, data)
                {
                    error!(
                        "Failed to read virtio-dev config space {} type: {} {:?}",
                        offset - 0x100,
                        self.device.lock().unwrap().device_type(),
                        e,
                    );
                    return false;
                }
            }
            _ => {
                warn!(
                    "Failed to read mmio register: overflows, offset is 0x{:x}, type: {}",
                    offset,
                    self.device.lock().unwrap().device_type(),
                );
            }
        };
        true
    }

    /// Write data by virtio driver from VM.
    fn write(&mut self, data: &[u8], _base: GuestAddress, offset: u64) -> bool {
        let mut locked_state = self.state.lock().unwrap();
        match offset {
            0x00..=0xff if data.len() == 4 => {
                let value = LittleEndian::read_u32(data);
                if let Err(ref e) = locked_state.config_space.write_common_config(
                    &self.device,
                    &self.interrupt_status,
                    offset,
                    value,
                ) {
                    error!(
                        "Failed to write mmio register {}, type: {}, {:?}",
                        offset,
                        self.device.lock().unwrap().device_type(),
                        e,
                    );
                    return false;
                }

                if locked_state.config_space.check_device_status(
                    CONFIG_STATUS_ACKNOWLEDGE
                        | CONFIG_STATUS_DRIVER
                        | CONFIG_STATUS_DRIVER_OK
                        | CONFIG_STATUS_FEATURES_OK,
                    CONFIG_STATUS_FAILED,
                ) && !locked_state.activated
                {
                    drop(locked_state);
                    if let Err(ref e) = self.activate() {
                        error!(
                            "Failed to activate dev, type: {}, {:?}",
                            self.device.lock().unwrap().device_type(),
                            e,
                        );
                        return false;
                    }
                    self.state.lock().unwrap().activated = true;
                }
            }
            0x100..=0xfff => {
                if locked_state
                    .config_space
                    .check_device_status(CONFIG_STATUS_DRIVER, CONFIG_STATUS_FAILED)
                {
                    if let Err(ref e) = self
                        .device
                        .lock()
                        .unwrap()
                        .write_config(offset - 0x100, data)
                    {
                        error!(
                            "Failed to write virtio-dev config space {}, type: {}, {:?}",
                            offset - 0x100,
                            self.device.lock().unwrap().device_type(),
                            e,
                        );
                        return false;
                    }
                } else {
                    error!("Failed to write virtio-dev config space: driver is not ready 0x{:X}, type: {}",
                        locked_state.config_space.get_device_status(),
                        self.device.lock().unwrap().device_type(),
                    );
                    return false;
                }
            }
            _ => {
                warn!(
                    "Failed to write mmio register: overflows, offset is 0x{:x} type: {}",
                    offset,
                    self.device.lock().unwrap().device_type(),
                );
                return false;
            }
        }
        true
    }

    fn ioeventfds(&self) -> Vec<RegionIoEventFd> {
        let mut ret = Vec::new();
        for (index, eventfd) in self.host_notify_info.events.iter().enumerate() {
            let addr = u64::from(NOTIFY_REG_OFFSET);
            ret.push(RegionIoEventFd {
                fd: eventfd.clone(),
                addr_range: AddressRange::from((addr, std::mem::size_of::<u32>() as u64)),
                data_match: true,
                data: index as u64,
            })
        }
        ret
    }

    fn interrupt_evt(&self) -> Option<&EventFd> {
        Some(self.interrupt_evt.as_ref())
    }

    fn get_sys_resource(&mut self) -> Option<&mut SysRes> {
        Some(&mut self.res)
    }

    fn get_type(&self) -> SysBusDevType {
        SysBusDevType::VirtioMmio
    }
}

impl acpi::AmlBuilder for VirtioMmioDevice {
    fn aml_bytes(&self) -> Vec<u8> {
        Vec::new()
    }
}

impl StateTransfer for VirtioMmioDevice {
    fn get_state_vec(&self) -> migration::Result<Vec<u8>> {
        let mut state = self.state.lock().unwrap();

        for (index, queue) in self.queues.iter().enumerate() {
            state.config_space.queues_config[index] =
                queue.lock().unwrap().vring.get_queue_config();
        }
        state.config_space.interrupt_status = self.interrupt_status.load(Ordering::Relaxed);

        Ok(state.as_bytes().to_vec())
    }

    fn set_state_mut(&mut self, state: &[u8]) -> migration::Result<()> {
        let s_len = std::mem::size_of::<VirtioMmioState>();
        if state.len() != s_len {
            bail!("Invalid state length {}, expected {}", state.len(), s_len);
        }
        let mut locked_state = self.state.lock().unwrap();
        locked_state.as_mut_bytes().copy_from_slice(state);
        let cloned_mem_space = self.mem_space.clone();
        let mut queue_states = locked_state.config_space.queues_config
            [0..locked_state.config_space.queue_num]
            .to_vec();
        self.queues = queue_states
            .iter_mut()
            .map(|queue_state| {
                queue_state.addr_cache.desc_table_host = cloned_mem_space
                    .get_host_address(queue_state.desc_table)
                    .unwrap_or(0);
                queue_state.addr_cache.avail_ring_host = cloned_mem_space
                    .get_host_address(queue_state.avail_ring)
                    .unwrap_or(0);
                queue_state.addr_cache.used_ring_host = cloned_mem_space
                    .get_host_address(queue_state.used_ring)
                    .unwrap_or(0);
                Arc::new(Mutex::new(
                    Queue::new(*queue_state, locked_state.config_space.queue_type).unwrap(),
                ))
            })
            .collect();
        self.interrupt_status
            .store(locked_state.config_space.interrupt_status, Ordering::SeqCst);

        Ok(())
    }

    fn get_device_alias(&self) -> u64 {
        MigrationManager::get_desc_alias(&VirtioMmioState::descriptor().name).unwrap_or(!0)
    }
}

impl MigrationHook for VirtioMmioDevice {
    fn resume(&mut self) -> migration::Result<()> {
        if self.state.lock().unwrap().activated {
            let mut queue_evts = Vec::<Arc<EventFd>>::new();
            for fd in self.host_notify_info.events.iter() {
                queue_evts.push(fd.clone());
            }

            if let Some(cb) = self.interrupt_cb.clone() {
                if let Err(e) = self.device.lock().unwrap().activate(
                    self.mem_space.clone(),
                    cb,
                    &self.queues,
                    queue_evts,
                ) {
                    bail!("Failed to resume virtio mmio device: {}", e);
                }
            } else {
                bail!("Failed to resume device: No interrupt callback");
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use address_space::{AddressSpace, GuestAddress, HostMemMapping, Region};
    use util::num_ops::read_u32;

    use super::*;
    use crate::VIRTIO_TYPE_BLOCK;

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

    const SYSTEM_SPACE_SIZE: u64 = (1024 * 1024) as u64;
    const CONFIG_SPACE_SIZE: usize = 16;
    const QUEUE_NUM: usize = 2;
    const QUEUE_SIZE: u16 = 256;

    pub struct VirtioDeviceTest {
        pub device_features: u64,
        pub driver_features: u64,
        pub config_space: Vec<u8>,
        pub b_active: bool,
        pub b_realized: bool,
    }

    impl VirtioDeviceTest {
        pub fn new() -> Self {
            let mut config_space = Vec::new();
            for i in 0..CONFIG_SPACE_SIZE {
                config_space.push(i as u8);
            }

            VirtioDeviceTest {
                device_features: 0,
                driver_features: 0,
                b_active: false,
                b_realized: false,
                config_space,
            }
        }
    }

    impl VirtioDevice for VirtioDeviceTest {
        fn realize(&mut self) -> Result<()> {
            self.b_realized = true;
            Ok(())
        }

        fn device_type(&self) -> u32 {
            VIRTIO_TYPE_BLOCK
        }

        fn queue_num(&self) -> usize {
            QUEUE_NUM
        }

        fn queue_size(&self) -> u16 {
            QUEUE_SIZE
        }

        fn get_device_features(&self, features_select: u32) -> u32 {
            read_u32(self.device_features, features_select)
        }

        fn set_driver_features(&mut self, page: u32, value: u32) {
            self.driver_features = self.checked_driver_features(page, value);
        }

        fn get_driver_features(&self, features_select: u32) -> u32 {
            read_u32(self.driver_features, features_select)
        }

        fn read_config(&self, offset: u64, mut data: &mut [u8]) -> Result<()> {
            let config_len = self.config_space.len() as u64;
            if offset >= config_len {
                bail!(
                    "The offset{} for reading is more than the length{} of configuration",
                    offset,
                    config_len
                );
            }
            if let Some(end) = offset.checked_add(data.len() as u64) {
                data.write_all(
                    &self.config_space[offset as usize..std::cmp::min(end, config_len) as usize],
                )?;
            }

            Ok(())
        }

        fn write_config(&mut self, offset: u64, data: &[u8]) -> Result<()> {
            let data_len = data.len();
            let config_len = self.config_space.len();
            if offset as usize + data_len > config_len {
                bail!(
                    "The offset{} {}for writing is more than the length{} of configuration",
                    offset,
                    data_len,
                    config_len
                );
            }

            self.config_space[(offset as usize)..(offset as usize + data_len)]
                .copy_from_slice(&data[..]);

            Ok(())
        }

        fn activate(
            &mut self,
            _mem_space: Arc<AddressSpace>,
            _interrupt_cb: Arc<VirtioInterrupt>,
            _queues: &[Arc<Mutex<Queue>>],
            mut _queue_evts: Vec<Arc<EventFd>>,
        ) -> Result<()> {
            self.b_active = true;
            Ok(())
        }
    }

    #[test]
    fn test_virtio_mmio_device_new() {
        let virtio_device = Arc::new(Mutex::new(VirtioDeviceTest::new()));
        let virtio_device_clone = virtio_device.clone();
        let sys_space = address_space_init();

        let virtio_mmio_device = VirtioMmioDevice::new(&sys_space, virtio_device);
        assert_eq!(virtio_mmio_device.state.lock().unwrap().activated, false);
        assert_eq!(
            virtio_mmio_device.host_notify_info.events.len(),
            virtio_device_clone.lock().unwrap().queue_num()
        );
        let config_space = virtio_mmio_device.state.lock().unwrap().config_space;
        assert_eq!(config_space.features_select, 0);
        assert_eq!(config_space.acked_features_select, 0);
        assert_eq!(config_space.device_status, 0);
        assert_eq!(config_space.config_generation, 0);
        assert_eq!(config_space.queue_select, 0);
        assert_eq!(
            config_space.queue_num,
            virtio_device_clone.lock().unwrap().queue_num()
        );
        assert_eq!(config_space.queue_type, QUEUE_TYPE_SPLIT_VRING);
    }

    #[test]
    fn test_virtio_mmio_device_read_01() {
        let virtio_device = Arc::new(Mutex::new(VirtioDeviceTest::new()));
        let virtio_device_clone = virtio_device.clone();
        let sys_space = address_space_init();
        let mut virtio_mmio_device = VirtioMmioDevice::new(&sys_space, virtio_device);
        let addr = GuestAddress(0);

        // read the register of magic value
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        assert_eq!(
            virtio_mmio_device.read(&mut buf[..], addr, MAGIC_VALUE_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&buf[..]), MMIO_MAGIC_VALUE);

        // read the register of version
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        assert_eq!(
            virtio_mmio_device.read(&mut buf[..], addr, VERSION_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&buf[..]), MMIO_VERSION);

        // read the register of device id
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        assert_eq!(
            virtio_mmio_device.read(&mut buf[..], addr, DEVICE_ID_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&buf[..]), VIRTIO_TYPE_BLOCK);

        // read the register of vendor id
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        assert_eq!(
            virtio_mmio_device.read(&mut buf[..], addr, VENDOR_ID_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&buf[..]), VENDOR_ID);

        // read the register of the features
        // get low 32bit of the features
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        virtio_mmio_device
            .state
            .lock()
            .unwrap()
            .config_space
            .features_select = 0;
        virtio_device_clone.lock().unwrap().device_features = 0x0000_00f8_0000_00fe;
        assert_eq!(
            virtio_mmio_device.read(&mut buf[..], addr, DEVICE_FEATURES_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&buf[..]), 0x0000_00fe);
        // get high 32bit of the features for device which supports VirtIO Version 1
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        virtio_mmio_device
            .state
            .lock()
            .unwrap()
            .config_space
            .features_select = 1;
        assert_eq!(
            virtio_mmio_device.read(&mut buf[..], addr, DEVICE_FEATURES_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&buf[..]), 0x0000_00f9);
    }

    #[test]
    fn test_virtio_mmio_device_read_02() {
        let virtio_device = Arc::new(Mutex::new(VirtioDeviceTest::new()));
        let sys_space = address_space_init();
        let mut virtio_mmio_device = VirtioMmioDevice::new(&sys_space, virtio_device);
        let addr = GuestAddress(0);

        // read the register representing max size of the queue
        // for queue_select as 0
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        virtio_mmio_device
            .state
            .lock()
            .unwrap()
            .config_space
            .queue_select = 0;
        assert_eq!(
            virtio_mmio_device.read(&mut buf[..], addr, QUEUE_NUM_MAX_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&buf[..]), QUEUE_SIZE as u32);
        // for queue_select as 1
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        virtio_mmio_device
            .state
            .lock()
            .unwrap()
            .config_space
            .queue_select = 1;
        assert_eq!(
            virtio_mmio_device.read(&mut buf[..], addr, QUEUE_NUM_MAX_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&buf[..]), QUEUE_SIZE as u32);

        // read the register representing the status of queue
        // for queue_select as 0
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        let mut locked_state = virtio_mmio_device.state.lock().unwrap();
        locked_state.config_space.queue_select = 0;
        locked_state.config_space.device_status = CONFIG_STATUS_FEATURES_OK;
        drop(locked_state);
        LittleEndian::write_u32(&mut buf[..], 1);
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, QUEUE_READY_REG),
            true
        );
        let mut data: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        assert_eq!(
            virtio_mmio_device.read(&mut data[..], addr, QUEUE_READY_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&data[..]), 1);
        // for queue_select as 1
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        let mut locked_state = virtio_mmio_device.state.lock().unwrap();
        locked_state.config_space.queue_select = 1;
        locked_state.config_space.device_status = CONFIG_STATUS_FEATURES_OK;
        drop(locked_state);
        assert_eq!(
            virtio_mmio_device.read(&mut buf[..], addr, QUEUE_READY_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&buf[..]), 0);

        // read the register representing the status of interrupt
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        assert_eq!(
            virtio_mmio_device.read(&mut buf[..], addr, INTERRUPT_STATUS_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&buf[..]), 0);
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        virtio_mmio_device
            .interrupt_status
            .store(0b10_1111, Ordering::Relaxed);
        assert_eq!(
            virtio_mmio_device.read(&mut buf[..], addr, INTERRUPT_STATUS_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&buf[..]), 0b10_1111);

        // read the register representing the status of device
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        virtio_mmio_device
            .state
            .lock()
            .unwrap()
            .config_space
            .device_status = 0;
        assert_eq!(
            virtio_mmio_device.read(&mut buf[..], addr, STATUS_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&buf[..]), 0);
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        virtio_mmio_device
            .state
            .lock()
            .unwrap()
            .config_space
            .device_status = 5;
        assert_eq!(
            virtio_mmio_device.read(&mut buf[..], addr, STATUS_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&buf[..]), 5);
    }

    #[test]
    fn test_virtio_mmio_device_read_03() {
        let virtio_device = Arc::new(Mutex::new(VirtioDeviceTest::new()));
        let virtio_device_clone = virtio_device.clone();
        let sys_space = address_space_init();
        let mut virtio_mmio_device = VirtioMmioDevice::new(&sys_space, virtio_device);
        let addr = GuestAddress(0);

        // read the configuration atomic value
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        assert_eq!(
            virtio_mmio_device.read(&mut buf[..], addr, CONFIG_GENERATION_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&buf[..]), 0);
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        virtio_mmio_device
            .state
            .lock()
            .unwrap()
            .config_space
            .config_generation = 10;
        assert_eq!(
            virtio_mmio_device.read(&mut buf[..], addr, CONFIG_GENERATION_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&buf[..]), 10);

        // read the unknown register
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        assert_eq!(virtio_mmio_device.read(&mut buf[..], addr, 0xf1), false);
        assert_eq!(virtio_mmio_device.read(&mut buf[..], addr, 0xfff + 1), true);
        assert_eq!(buf, [0xff, 0xff, 0xff, 0xff]);

        // read the configuration space of virtio device
        // write something
        let result: Vec<u8> = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        virtio_device_clone
            .lock()
            .unwrap()
            .config_space
            .as_mut_slice()
            .copy_from_slice(&result[..]);

        let mut data: Vec<u8> = vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(virtio_mmio_device.read(&mut data[..], addr, 0x100), true);
        assert_eq!(data, result);

        let mut data: Vec<u8> = vec![0, 0, 0, 0, 0, 0, 0, 0];
        let result: Vec<u8> = vec![9, 10, 11, 12, 13, 14, 15, 16];
        assert_eq!(virtio_mmio_device.read(&mut data[..], addr, 0x108), true);
        assert_eq!(data, result);
    }

    #[test]
    fn test_virtio_mmio_device_write_01() {
        let virtio_device = Arc::new(Mutex::new(VirtioDeviceTest::new()));
        let virtio_device_clone = virtio_device.clone();
        let sys_space = address_space_init();
        let mut virtio_mmio_device = VirtioMmioDevice::new(&sys_space, virtio_device);
        let addr = GuestAddress(0);

        // write the selector for device features
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        LittleEndian::write_u32(&mut buf[..], 2);
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, DEVICE_FEATURES_SEL_REG),
            true
        );
        assert_eq!(
            virtio_mmio_device
                .state
                .lock()
                .unwrap()
                .config_space
                .features_select,
            2
        );

        // write the device features
        // false when the device status is CONFIG_STATUS_FEATURES_OK or CONFIG_STATUS_FAILED isn't CONFIG_STATUS_DRIVER
        virtio_mmio_device
            .state
            .lock()
            .unwrap()
            .config_space
            .device_status = CONFIG_STATUS_FEATURES_OK;
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, DRIVER_FEATURES_REG),
            false
        );
        virtio_mmio_device
            .state
            .lock()
            .unwrap()
            .config_space
            .device_status = CONFIG_STATUS_FAILED;
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, DRIVER_FEATURES_REG),
            false
        );
        virtio_mmio_device
            .state
            .lock()
            .unwrap()
            .config_space
            .device_status =
            CONFIG_STATUS_FEATURES_OK | CONFIG_STATUS_FAILED | CONFIG_STATUS_DRIVER;
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, DRIVER_FEATURES_REG),
            false
        );
        // it is ok to write the low 32bit of device features
        let mut locked_state = virtio_mmio_device.state.lock().unwrap();
        locked_state.config_space.device_status = CONFIG_STATUS_DRIVER;
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        locked_state.config_space.acked_features_select = 0;
        drop(locked_state);
        LittleEndian::write_u32(&mut buf[..], 0x0000_00fe);
        virtio_device_clone.lock().unwrap().device_features = 0x0000_00fe;
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, DRIVER_FEATURES_REG),
            true
        );
        assert_eq!(
            virtio_device_clone.lock().unwrap().driver_features as u32,
            0x0000_00fe
        );
        // it is ok to write the high 32bit of device features
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        virtio_mmio_device
            .state
            .lock()
            .unwrap()
            .config_space
            .acked_features_select = 1;
        LittleEndian::write_u32(&mut buf[..], 0x0000_00ff);
        virtio_device_clone.lock().unwrap().device_features = 0x0000_00ff_0000_0000;
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, DRIVER_FEATURES_REG),
            true
        );
        assert_eq!(
            virtio_mmio_device
                .state
                .lock()
                .unwrap()
                .config_space
                .queue_type,
            QUEUE_TYPE_PACKED_VRING
        );
        assert_eq!(
            virtio_device_clone.lock().unwrap().driver_features >> 32 as u32,
            0x0000_00ff
        );

        // write the selector of driver features
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        LittleEndian::write_u32(&mut buf[..], 0x00ff_0000);
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, DRIVER_FEATURES_SEL_REG),
            true
        );
        assert_eq!(
            virtio_mmio_device
                .state
                .lock()
                .unwrap()
                .config_space
                .acked_features_select,
            0x00ff_0000
        );

        // write the selector of queue
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        LittleEndian::write_u32(&mut buf[..], 0x0000_ff00);
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, QUEUE_SEL_REG),
            true
        );
        assert_eq!(
            virtio_mmio_device
                .state
                .lock()
                .unwrap()
                .config_space
                .queue_select,
            0x0000_ff00
        );

        // write the size of queue
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        let mut locked_state = virtio_mmio_device.state.lock().unwrap();
        locked_state.config_space.queue_select = 0;
        locked_state.config_space.device_status = CONFIG_STATUS_FEATURES_OK;
        drop(locked_state);
        LittleEndian::write_u32(&mut buf[..], 128);
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, QUEUE_NUM_REG),
            true
        );
        let locked_state = virtio_mmio_device.state.lock().unwrap();
        if let Ok(config) = locked_state.config_space.get_queue_config() {
            assert_eq!(config.size, 128);
        } else {
            assert!(false);
        }
    }

    #[test]
    fn test_virtio_mmio_device_write_02() {
        let virtio_device = Arc::new(Mutex::new(VirtioDeviceTest::new()));
        let sys_space = address_space_init();
        let mut virtio_mmio_device = VirtioMmioDevice::new(&sys_space, virtio_device);
        let addr = GuestAddress(0);

        // write the ready status of queue
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        let mut locked_state = virtio_mmio_device.state.lock().unwrap();
        locked_state.config_space.queue_select = 0;
        locked_state.config_space.device_status = CONFIG_STATUS_FEATURES_OK;
        drop(locked_state);
        LittleEndian::write_u32(&mut buf[..], 1);
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, QUEUE_READY_REG),
            true
        );
        let mut data: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        assert_eq!(
            virtio_mmio_device.read(&mut data[..], addr, QUEUE_READY_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&data[..]), 1);

        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        let mut locked_state = virtio_mmio_device.state.lock().unwrap();
        locked_state.config_space.queue_select = 0;
        locked_state.config_space.device_status = CONFIG_STATUS_FEATURES_OK;
        drop(locked_state);
        LittleEndian::write_u32(&mut buf[..], 2);
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, QUEUE_READY_REG),
            true
        );
        let mut data: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        assert_eq!(
            virtio_mmio_device.read(&mut data[..], addr, QUEUE_READY_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&data[..]), 0);

        // write the interrupt status
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        virtio_mmio_device
            .state
            .lock()
            .unwrap()
            .config_space
            .device_status = CONFIG_STATUS_DRIVER_OK;
        virtio_mmio_device
            .interrupt_status
            .store(0b10_1111, Ordering::Relaxed);
        LittleEndian::write_u32(&mut buf[..], 0b111);
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, INTERRUPT_ACK_REG),
            true
        );
        let mut data: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        assert_eq!(
            virtio_mmio_device.read(&mut data[..], addr, INTERRUPT_STATUS_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&data[..]), 0b10_1000);
    }

    #[test]
    fn test_virtio_mmio_device_write_03() {
        let virtio_device = Arc::new(Mutex::new(VirtioDeviceTest::new()));
        let sys_space = address_space_init();
        let mut virtio_mmio_device = VirtioMmioDevice::new(&sys_space, virtio_device);
        let addr = GuestAddress(0);

        // write the low 32bit of queue's descriptor table address
        let mut locked_state = virtio_mmio_device.state.lock().unwrap();
        locked_state.config_space.queue_select = 0;
        locked_state.config_space.device_status = CONFIG_STATUS_FEATURES_OK;
        drop(locked_state);
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        LittleEndian::write_u32(&mut buf[..], 0xffff_fefe);
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, QUEUE_DESC_LOW_REG),
            true
        );
        if let Ok(config) = virtio_mmio_device
            .state
            .lock()
            .unwrap()
            .config_space
            .get_queue_config()
        {
            assert_eq!(config.desc_table.0 as u32, 0xffff_fefe)
        } else {
            assert!(false);
        }

        // write the high 32bit of queue's descriptor table address
        let mut locked_state = virtio_mmio_device.state.lock().unwrap();
        locked_state.config_space.queue_select = 0;
        locked_state.config_space.device_status = CONFIG_STATUS_FEATURES_OK;
        drop(locked_state);
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        LittleEndian::write_u32(&mut buf[..], 0xfcfc_ffff);
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, QUEUE_DESC_HIGH_REG),
            true
        );
        if let Ok(config) = virtio_mmio_device
            .state
            .lock()
            .unwrap()
            .config_space
            .get_queue_config()
        {
            assert_eq!((config.desc_table.0 >> 32) as u32, 0xfcfc_ffff)
        } else {
            assert!(false);
        }

        // write the low 32bit of queue's available ring address
        let mut locked_state = virtio_mmio_device.state.lock().unwrap();
        locked_state.config_space.queue_select = 0;
        locked_state.config_space.device_status = CONFIG_STATUS_FEATURES_OK;
        drop(locked_state);
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        LittleEndian::write_u32(&mut buf[..], 0xfcfc_fafa);
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, QUEUE_AVAIL_LOW_REG),
            true
        );
        if let Ok(config) = virtio_mmio_device
            .state
            .lock()
            .unwrap()
            .config_space
            .get_queue_config()
        {
            assert_eq!(config.avail_ring.0 as u32, 0xfcfc_fafa)
        } else {
            assert!(false);
        }

        // write the high 32bit of queue's available ring address
        let mut locked_state = virtio_mmio_device.state.lock().unwrap();
        locked_state.config_space.queue_select = 0;
        locked_state.config_space.device_status = CONFIG_STATUS_FEATURES_OK;
        drop(locked_state);
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        LittleEndian::write_u32(&mut buf[..], 0xecec_fafa);
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, QUEUE_AVAIL_HIGH_REG),
            true
        );
        if let Ok(config) = virtio_mmio_device
            .state
            .lock()
            .unwrap()
            .config_space
            .get_queue_config()
        {
            assert_eq!((config.avail_ring.0 >> 32) as u32, 0xecec_fafa)
        } else {
            assert!(false);
        }

        // write the low 32bit of queue's used ring address
        let mut locked_state = virtio_mmio_device.state.lock().unwrap();
        locked_state.config_space.queue_select = 0;
        locked_state.config_space.device_status = CONFIG_STATUS_FEATURES_OK;
        drop(locked_state);
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        LittleEndian::write_u32(&mut buf[..], 0xacac_fafa);
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, QUEUE_USED_LOW_REG),
            true
        );
        if let Ok(config) = virtio_mmio_device
            .state
            .lock()
            .unwrap()
            .config_space
            .get_queue_config()
        {
            assert_eq!(config.used_ring.0 as u32, 0xacac_fafa)
        } else {
            assert!(false);
        }

        // write the high 32bit of queue's used ring address
        let mut locked_state = virtio_mmio_device.state.lock().unwrap();
        locked_state.config_space.queue_select = 0;
        locked_state.config_space.device_status = CONFIG_STATUS_FEATURES_OK;
        drop(locked_state);
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        LittleEndian::write_u32(&mut buf[..], 0xcccc_fafa);
        assert_eq!(
            virtio_mmio_device.write(&buf[..], addr, QUEUE_USED_HIGH_REG),
            true
        );
        let locked_state = virtio_mmio_device.state.lock().unwrap();
        if let Ok(config) = locked_state.config_space.get_queue_config() {
            assert_eq!((config.used_ring.0 >> 32) as u32, 0xcccc_fafa)
        } else {
            assert!(false);
        }
    }

    fn align(size: u64, alignment: u64) -> u64 {
        let align_adjust = if size % alignment != 0 {
            alignment - (size % alignment)
        } else {
            0
        };
        (size + align_adjust) as u64
    }

    #[test]
    fn test_virtio_mmio_device_write_04() {
        let virtio_device = Arc::new(Mutex::new(VirtioDeviceTest::new()));
        let virtio_device_clone = virtio_device.clone();
        let sys_space = address_space_init();
        let mut virtio_mmio_device = VirtioMmioDevice::new(&sys_space, virtio_device);
        let addr = GuestAddress(0);

        virtio_mmio_device.assign_interrupt_cb();
        let mut locked_state = virtio_mmio_device.state.lock().unwrap();
        locked_state.config_space.queue_select = 0;
        locked_state.config_space.device_status = CONFIG_STATUS_FEATURES_OK;
        if let Ok(config) = locked_state.config_space.get_mut_queue_config() {
            config.desc_table = GuestAddress(0);
            config.avail_ring = GuestAddress((QUEUE_SIZE as u64) * 16);
            config.used_ring = GuestAddress(align(
                (QUEUE_SIZE as u64) * 16 + 8 + 2 * (QUEUE_SIZE as u64),
                4096,
            ));
            config.size = QUEUE_SIZE;
            config.ready = true;
        }
        locked_state.config_space.queue_select = 1;
        if let Ok(config) = locked_state.config_space.get_mut_queue_config() {
            config.desc_table = GuestAddress(0);
            config.avail_ring = GuestAddress((QUEUE_SIZE as u64) * 16);
            config.used_ring = GuestAddress(align(
                (QUEUE_SIZE as u64) * 16 + 8 + 2 * (QUEUE_SIZE as u64),
                4096,
            ));
            config.size = QUEUE_SIZE / 2;
            config.ready = true;
        }
        drop(locked_state);

        // write the device status
        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        LittleEndian::write_u32(&mut buf[..], CONFIG_STATUS_ACKNOWLEDGE);
        assert_eq!(virtio_mmio_device.write(&buf[..], addr, STATUS_REG), true);
        assert_eq!(virtio_mmio_device.state.lock().unwrap().activated, false);
        let mut data: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        assert_eq!(
            virtio_mmio_device.read(&mut data[..], addr, STATUS_REG),
            true
        );
        assert_eq!(LittleEndian::read_u32(&data[..]), CONFIG_STATUS_ACKNOWLEDGE);

        let mut buf: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        LittleEndian::write_u32(
            &mut buf[..],
            CONFIG_STATUS_ACKNOWLEDGE
                | CONFIG_STATUS_DRIVER
                | CONFIG_STATUS_DRIVER_OK
                | CONFIG_STATUS_FEATURES_OK,
        );
        assert_eq!(virtio_device_clone.lock().unwrap().b_active, false);
        assert_eq!(virtio_mmio_device.write(&buf[..], addr, STATUS_REG), true);
        assert_eq!(virtio_mmio_device.state.lock().unwrap().activated, true);
        assert_eq!(virtio_device_clone.lock().unwrap().b_active, true);
        let mut data: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        assert_eq!(
            virtio_mmio_device.read(&mut data[..], addr, STATUS_REG),
            true
        );
        assert_eq!(
            LittleEndian::read_u32(&data[..]),
            CONFIG_STATUS_ACKNOWLEDGE
                | CONFIG_STATUS_DRIVER
                | CONFIG_STATUS_DRIVER_OK
                | CONFIG_STATUS_FEATURES_OK
        );
    }
}
