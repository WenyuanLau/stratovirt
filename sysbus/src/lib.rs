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

#[macro_use]
extern crate error_chain;
#[macro_use]
extern crate log;
extern crate kvm_ioctls;

pub mod errors {
    error_chain! {
        foreign_links {
            KvmIoctl(kvm_ioctls::Error);
        }
    }
}

use std::sync::{Arc, Mutex};

use address_space::{AddressSpace, GuestAddress, Region, RegionIoEventFd, RegionOps};
use error_chain::ChainedError;
use kvm_ioctls::VmFd;
use vmm_sys_util::eventfd::EventFd;

use errors::{Result, ResultExt};

pub struct SysBus {
    #[cfg(target_arch = "x86_64")]
    pub sys_io: Arc<AddressSpace>,
    pub sys_mem: Arc<AddressSpace>,
    pub devices: Vec<Arc<Mutex<dyn SysBusDevOps>>>,
    pub free_irqs: (i32, i32),
    pub min_free_irq: i32,
    pub mmio_region: (u64, u64),
    pub min_free_base: u64,
}

impl SysBus {
    pub fn new(
        #[cfg(target_arch = "x86_64")] sys_io: &Arc<AddressSpace>,
        sys_mem: &Arc<AddressSpace>,
        free_irqs: (i32, i32),
        mmio_region: (u64, u64),
    ) -> Self {
        Self {
            #[cfg(target_arch = "x86_64")]
            sys_io: sys_io.clone(),
            sys_mem: sys_mem.clone(),
            devices: Vec::new(),
            free_irqs,
            min_free_irq: free_irqs.0,
            mmio_region,
            min_free_base: mmio_region.0,
        }
    }

    pub fn build_region_ops<T: 'static + SysBusDevOps>(&self, dev: &Arc<Mutex<T>>) -> RegionOps {
        let cloned_dev = dev.clone();
        let read_ops = move |data: &mut [u8], addr: GuestAddress, offset: u64| -> bool {
            cloned_dev.lock().unwrap().read(data, addr, offset)
        };

        let cloned_dev = dev.clone();
        let write_ops = move |data: &[u8], addr: GuestAddress, offset: u64| -> bool {
            cloned_dev.lock().unwrap().write(data, addr, offset)
        };

        RegionOps {
            read: Arc::new(read_ops),
            write: Arc::new(write_ops),
        }
    }

    pub fn attach_device<T: 'static + SysBusDevOps>(
        &mut self,
        dev: &Arc<Mutex<T>>,
        region_base: u64,
        region_size: u64,
    ) -> Result<()> {
        let region_ops = self.build_region_ops(dev);
        let region = Region::init_io_region(region_size, region_ops);
        let locked_dev = dev.lock().unwrap();

        region.set_ioeventfds(&locked_dev.ioeventfds());
        match locked_dev.get_type() {
            SysBusDevType::Serial if cfg!(target_arch = "x86_64") => {
                #[cfg(target_arch = "x86_64")]
                if let Err(e) = self.sys_io.root().add_subregion(region, region_base) {
                    error!("{}", e.display_chain());
                    bail!(
                        "Failed to register region in I/O space: offset={},size={}",
                        region_base,
                        region_size
                    );
                }
            }
            _ => {
                if let Err(e) = self.sys_mem.root().add_subregion(region, region_base) {
                    error!("{}", e.display_chain());
                    bail!(
                        "Failed to register region in memory space: offset={},size={}",
                        region_base,
                        region_size
                    );
                }
            }
        }
        self.devices.push(dev.clone());
        Ok(())
    }
}

pub struct SysRes {
    pub region_base: u64,
    pub region_size: u64,
    pub irq: i32,
}

impl Default for SysRes {
    fn default() -> Self {
        Self {
            region_base: 0,
            region_size: 0,
            irq: -1,
        }
    }
}

pub enum SysBusDevType {
    Serial,
    #[cfg(target_arch = "aarch64")]
    Rtc,
    VirtioMmio,
    Others,
}

/// Operations for sysbus devices.
pub trait SysBusDevOps: Send {
    /// Read function of device.
    ///
    /// # Arguments
    ///
    /// * `data` - A u8-type array.
    /// * `base` - Base address of this device.
    /// * `offset` - Offset from base address.
    fn read(&mut self, data: &mut [u8], base: GuestAddress, offset: u64) -> bool;

    /// Write function of device.
    ///
    /// # Arguments
    ///
    /// * `data` - A u8-type array.
    /// * `base` - Base address of this device.
    /// * `offset` - Offset from base address.
    fn write(&mut self, data: &[u8], base: GuestAddress, offset: u64) -> bool;

    fn ioeventfds(&self) -> Vec<RegionIoEventFd> {
        Vec::new()
    }

    fn interrupt_evt(&self) -> Option<&EventFd> {
        None
    }

    fn set_irq(&mut self, sysbus: &mut SysBus, vm_fd: &VmFd) -> Result<i32> {
        let irq = sysbus.min_free_irq;
        if irq > sysbus.free_irqs.1 {
            bail!("IRQ number exhausted.");
        }
        vm_fd
            .register_irqfd(self.interrupt_evt().unwrap(), irq as u32)
            .chain_err(|| "Failed to register irqfd")?;
        sysbus.min_free_irq = irq + 1;
        Ok(irq)
    }

    fn get_sys_resource(&mut self) -> &mut SysRes;

    fn set_sys_resource(
        &mut self,
        sysbus: &mut SysBus,
        region_base: u64,
        region_size: u64,
        vm_fd: &VmFd,
    ) -> Result<()> {
        let irq = self.set_irq(sysbus, vm_fd)?;
        let res = self.get_sys_resource();
        res.region_base = region_base;
        res.region_size = region_size;
        res.irq = irq;
        Ok(())
    }

    fn get_type(&self) -> SysBusDevType {
        SysBusDevType::Others
    }
}