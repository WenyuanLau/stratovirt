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

use std::sync::{Arc, Mutex, Weak};

use address_space::{Region, RegionOps};
use anyhow::{bail, Result};
use log::error;
use pci::{
    config::{
        PciConfig, CLASS_CODE_HOST_BRIDGE, DEVICE_ID, PCI_CONFIG_SPACE_SIZE, SUB_CLASS_CODE,
        VENDOR_ID,
    },
    le_read_u64, le_write_u16, ranges_overlap, PciBus, PciDevOps, Result as PciResult,
};

use super::VENDOR_ID_INTEL;

const DEVICE_ID_INTEL_Q35_MCH: u16 = 0x29c0;

const PCIEXBAR: u8 = 0x60;
const PCIEXBAR_ENABLE_MASK: u64 = 0x1;
const PCIEXBAR_ADDR_MASK: u64 = 0xf_f000_0000;
const PCIEXBAR_LENGTH_MASK: u64 = 0x6;
const PCIEXBAR_LENGTH_256MB: u64 = 0x0;
const PCIEXBAR_LENGTH_128MB: u64 = 0x2;
const PCIEXBAR_LENGTH_64MB: u64 = 0x4;
const PCIEXBAR_128MB_ADDR_MASK: u64 = 1 << 26;
const PCIEXBAR_64MB_ADDR_MASK: u64 = 1 << 25;
// Bit 25:3 of PCIEXBAR is reserved.
const PCIEXBAR_RESERVED_MASK: u64 = 0x3ff_fff8;

/// Memory controller hub (Device 0:Function 0)
pub struct Mch {
    config: PciConfig,
    parent_bus: Weak<Mutex<PciBus>>,
    mmconfig_region: Option<Region>,
    mmconfig_ops: RegionOps,
}

impl Mch {
    pub fn new(
        parent_bus: Weak<Mutex<PciBus>>,
        mmconfig_region: Region,
        mmconfig_ops: RegionOps,
    ) -> Self {
        Self {
            config: PciConfig::new(PCI_CONFIG_SPACE_SIZE, 0),
            parent_bus,
            mmconfig_region: Some(mmconfig_region),
            mmconfig_ops,
        }
    }

    fn update_pciexbar_mapping(&mut self) -> Result<()> {
        let pciexbar: u64 = le_read_u64(&self.config.config, PCIEXBAR as usize)?;
        let enable = pciexbar & PCIEXBAR_ENABLE_MASK;
        let length: u64;
        let mut addr_mask: u64 = PCIEXBAR_ADDR_MASK;
        match pciexbar & PCIEXBAR_LENGTH_MASK {
            PCIEXBAR_LENGTH_256MB => length = 256 << 20,
            PCIEXBAR_LENGTH_128MB => {
                length = 128 << 20;
                addr_mask |= PCIEXBAR_128MB_ADDR_MASK;
            }
            PCIEXBAR_LENGTH_64MB => {
                length = 64 << 20;
                addr_mask |= PCIEXBAR_128MB_ADDR_MASK | PCIEXBAR_64MB_ADDR_MASK;
            }
            _ => bail!("Invalid PCIEXBAR length."),
        }

        if let Some(region) = self.mmconfig_region.as_ref() {
            self.parent_bus
                .upgrade()
                .unwrap()
                .lock()
                .unwrap()
                .mem_region
                .delete_subregion(region)?;
            self.mmconfig_region = None;
        }
        if enable == 0x1 {
            let region = Region::init_io_region(length, self.mmconfig_ops.clone());
            let base_addr: u64 = pciexbar & addr_mask;
            self.parent_bus
                .upgrade()
                .unwrap()
                .lock()
                .unwrap()
                .mem_region
                .add_subregion(region, base_addr)?;
        }
        Ok(())
    }

    fn check_pciexbar_update(&self, old_pciexbar: u64) -> bool {
        let cur_pciexbar: u64 = le_read_u64(&self.config.config, PCIEXBAR as usize).unwrap();

        if (cur_pciexbar & !PCIEXBAR_RESERVED_MASK) == (old_pciexbar & !PCIEXBAR_RESERVED_MASK) {
            return false;
        }
        true
    }
}

impl PciDevOps for Mch {
    fn init_write_mask(&mut self) -> PciResult<()> {
        self.config.init_common_write_mask()
    }

    fn init_write_clear_mask(&mut self) -> PciResult<()> {
        self.config.init_common_write_clear_mask()
    }

    fn realize(mut self) -> PciResult<()> {
        self.init_write_mask()?;
        self.init_write_clear_mask()?;

        le_write_u16(&mut self.config.config, VENDOR_ID as usize, VENDOR_ID_INTEL)?;
        le_write_u16(
            &mut self.config.config,
            DEVICE_ID as usize,
            DEVICE_ID_INTEL_Q35_MCH,
        )?;
        le_write_u16(
            &mut self.config.config,
            SUB_CLASS_CODE as usize,
            CLASS_CODE_HOST_BRIDGE,
        )?;

        let parent_bus = self.parent_bus.clone();
        parent_bus
            .upgrade()
            .unwrap()
            .lock()
            .unwrap()
            .devices
            .insert(0, Arc::new(Mutex::new(self)));
        Ok(())
    }

    fn read_config(&mut self, offset: usize, data: &mut [u8]) {
        self.config.read(offset, data);
    }

    fn write_config(&mut self, offset: usize, data: &[u8]) {
        let old_pciexbar: u64 = le_read_u64(&self.config.config, PCIEXBAR as usize).unwrap();
        self.config.write(offset, data, 0, None, None);

        if ranges_overlap(offset, data.len(), PCIEXBAR as usize, 8)
            && self.check_pciexbar_update(old_pciexbar)
        {
            if let Err(e) = self.update_pciexbar_mapping() {
                error!("{:?}", e);
            }
        }
    }

    fn name(&self) -> String {
        "Memory Controller Hub".to_string()
    }
}
