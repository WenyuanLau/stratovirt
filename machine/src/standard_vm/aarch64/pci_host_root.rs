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

use pci::{
    config::{
        PciConfig, CLASS_CODE_HOST_BRIDGE, DEVICE_ID, PCI_CONFIG_SPACE_SIZE, PCI_VENDOR_ID_REDHAT,
        REVISION_ID, SUB_CLASS_CODE, VENDOR_ID,
    },
    le_write_u16, PciBus, PciDevOps, Result as PciResult,
};

const DEVICE_ID_PCIE_HOST: u16 = 0x0008;

/// PciHost root (Device 0:Function 0).
pub struct PciHostRoot {
    /// Pci config space.
    config: PciConfig,
    /// Primary Bus.
    parent_bus: Weak<Mutex<PciBus>>,
}

impl PciHostRoot {
    pub fn new(parent_bus: Weak<Mutex<PciBus>>) -> Self {
        Self {
            config: PciConfig::new(PCI_CONFIG_SPACE_SIZE, 0),
            parent_bus,
        }
    }
}

impl PciDevOps for PciHostRoot {
    fn init_write_mask(&mut self) -> PciResult<()> {
        self.config.init_common_write_mask()
    }

    fn init_write_clear_mask(&mut self) -> PciResult<()> {
        self.config.init_common_write_clear_mask()
    }

    fn realize(mut self) -> PciResult<()> {
        self.init_write_mask()?;
        self.init_write_clear_mask()?;

        le_write_u16(
            &mut self.config.config,
            VENDOR_ID as usize,
            PCI_VENDOR_ID_REDHAT,
        )?;
        le_write_u16(
            &mut self.config.config,
            DEVICE_ID as usize,
            DEVICE_ID_PCIE_HOST,
        )?;
        le_write_u16(
            &mut self.config.config,
            SUB_CLASS_CODE as usize,
            CLASS_CODE_HOST_BRIDGE,
        )?;
        le_write_u16(&mut self.config.config, REVISION_ID, 0)?;

        let parent_bus = self.parent_bus.upgrade().unwrap();
        parent_bus
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
        self.config.write(offset, data, 0, None);
    }

    fn name(&self) -> String {
        "PCI Host Root".to_string()
    }
}
