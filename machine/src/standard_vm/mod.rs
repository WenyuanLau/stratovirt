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

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
#[cfg(target_arch = "x86_64")]
mod x86_64;

pub mod error;
pub use error::StandardVmError;

#[cfg(target_arch = "aarch64")]
pub use aarch64::StdMachine;
use log::error;
use machine_manager::event_loop::EventLoop;
use machine_manager::qmp::qmp_schema::UpdateRegionArgument;
#[cfg(not(target_env = "musl"))]
use ui::{
    input::{key_event, point_event},
    vnc::qmp_query_vnc,
};
use util::aio::{AioEngine, WriteZeroesState};
use util::loop_context::{read_fd, EventNotifier, NotifierCallback, NotifierOperation};
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::eventfd::EventFd;
#[cfg(target_arch = "x86_64")]
pub use x86_64::StdMachine;

use std::mem::size_of;
use std::ops::Deref;
use std::os::unix::io::RawFd;
use std::os::unix::prelude::AsRawFd;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use super::Result as MachineResult;
use crate::MachineOps;
#[cfg(target_arch = "x86_64")]
use acpi::AcpiGenericAddress;
use acpi::{
    AcpiRsdp, AcpiTable, AmlBuilder, TableLoader, ACPI_RSDP_FILE, ACPI_TABLE_FILE,
    ACPI_TABLE_LOADER_FILE, TABLE_CHECKSUM_OFFSET,
};
use address_space::{
    AddressRange, FileBackend, GuestAddress, HostMemMapping, Region, RegionIoEventFd, RegionOps,
};
pub use anyhow::Result;
use anyhow::{bail, Context};
use cpu::{CpuTopology, CPU};
use devices::legacy::FwCfgOps;
use machine_manager::config::{
    get_chardev_config, get_netdev_config, get_pci_df, BlkDevConfig, ChardevType, ConfigCheck,
    DriveConfig, ExBool, NetworkInterfaceConfig, NumaNode, NumaNodes, PciBdf, ScsiCntlrConfig,
    VmConfig, DEFAULT_VIRTQUEUE_SIZE, MAX_VIRTIO_QUEUE,
};
use machine_manager::machine::{DeviceInterface, KvmVmState};
use machine_manager::qmp::{qmp_schema, QmpChannel, Response};
use migration::MigrationManager;
use pci::hotplug::{handle_plug, handle_unplug_pci_request};
use pci::PciBus;
use util::byte_code::ByteCode;
use virtio::{
    qmp_balloon, qmp_query_balloon, Block, BlockState,
    ScsiCntlr::{scsi_cntlr_create_scsi_bus, ScsiCntlr},
    VhostKern, VhostUser, VirtioDevice, VirtioNetState, VirtioPciDevice,
};

#[cfg(target_arch = "aarch64")]
use aarch64::{LayoutEntryType, MEM_LAYOUT};
#[cfg(target_arch = "x86_64")]
use x86_64::{LayoutEntryType, MEM_LAYOUT};

#[cfg(target_arch = "x86_64")]
use self::x86_64::ich9_lpc::{PM_CTRL_OFFSET, PM_EVENT_OFFSET, RST_CTRL_OFFSET, SLEEP_CTRL_OFFSET};

trait StdMachineOps: AcpiBuilder {
    fn init_pci_host(&self) -> Result<()>;

    /// Build all ACPI tables and RSDP, and add them to FwCfg as file entries.
    ///
    /// # Arguments
    ///
    /// `fw_cfg` - FwCfgOps trait object.
    fn build_acpi_tables(&self, fw_cfg: &Arc<Mutex<dyn FwCfgOps>>) -> Result<()>
    where
        Self: Sized,
    {
        let mut loader = TableLoader::new();
        let acpi_tables = Arc::new(Mutex::new(Vec::new()));
        loader.add_alloc_entry(ACPI_TABLE_FILE, acpi_tables.clone(), 64_u32, false)?;

        let mut xsdt_entries = Vec::new();

        #[cfg(target_arch = "x86_64")]
        {
            let facs_addr = Self::build_facs_table(&acpi_tables, &mut loader)
                .with_context(|| "Failed to build ACPI FACS table")?;
            xsdt_entries.push(facs_addr);
        }

        let dsdt_addr = self
            .build_dsdt_table(&acpi_tables, &mut loader)
            .with_context(|| "Failed to build ACPI DSDT table")?;
        let fadt_addr = Self::build_fadt_table(&acpi_tables, &mut loader, dsdt_addr)
            .with_context(|| "Failed to build ACPI FADT table")?;
        xsdt_entries.push(fadt_addr);

        let madt_addr = self
            .build_madt_table(&acpi_tables, &mut loader)
            .with_context(|| "Failed to build ACPI MADT table")?;
        xsdt_entries.push(madt_addr);

        #[cfg(target_arch = "aarch64")]
        {
            let gtdt_addr = self
                .build_gtdt_table(&acpi_tables, &mut loader)
                .with_context(|| "Failed to build ACPI GTDT table")?;
            xsdt_entries.push(gtdt_addr);

            let iort_addr = self
                .build_iort_table(&acpi_tables, &mut loader)
                .with_context(|| "Failed to build ACPI IORT table")?;
            xsdt_entries.push(iort_addr);

            let spcr_addr = self
                .build_spcr_table(&acpi_tables, &mut loader)
                .with_context(|| "Failed to build ACPI SPCR table")?;
            xsdt_entries.push(spcr_addr);
        }

        let mcfg_addr = Self::build_mcfg_table(&acpi_tables, &mut loader)
            .with_context(|| "Failed to build ACPI MCFG table")?;
        xsdt_entries.push(mcfg_addr);

        if let Some(numa_nodes) = self.get_numa_nodes() {
            let srat_addr = self
                .build_srat_table(&acpi_tables, &mut loader)
                .with_context(|| "Failed to build ACPI SRAT table")?;
            xsdt_entries.push(srat_addr);

            let slit_addr = Self::build_slit_table(numa_nodes, &acpi_tables, &mut loader)
                .with_context(|| "Failed to build ACPI SLIT table")?;
            xsdt_entries.push(slit_addr);
        }

        #[cfg(target_arch = "aarch64")]
        {
            let pptt_addr = self
                .build_pptt_table(&acpi_tables, &mut loader)
                .with_context(|| "Failed to build ACPI PPTT table")?;
            xsdt_entries.push(pptt_addr);
        }

        let xsdt_addr = Self::build_xsdt_table(&acpi_tables, &mut loader, xsdt_entries)?;

        let mut locked_fw_cfg = fw_cfg.lock().unwrap();
        Self::build_rsdp(
            &mut loader,
            &mut *locked_fw_cfg as &mut dyn FwCfgOps,
            xsdt_addr,
        )
        .with_context(|| "Failed to build ACPI RSDP")?;

        locked_fw_cfg
            .add_file_entry(ACPI_TABLE_LOADER_FILE, loader.cmd_entries())
            .with_context(|| "Failed to add ACPI table loader file entry")?;
        locked_fw_cfg
            .add_file_entry(ACPI_TABLE_FILE, acpi_tables.lock().unwrap().to_vec())
            .with_context(|| "Failed to add ACPI-tables file entry")?;

        Ok(())
    }

    fn add_fwcfg_device(&mut self, _nr_cpus: u8) -> Result<Option<Arc<Mutex<dyn FwCfgOps>>>> {
        bail!("Not implemented");
    }

    fn get_cpu_topo(&self) -> &CpuTopology;

    fn get_cpus(&self) -> &Vec<Arc<CPU>>;

    fn get_numa_nodes(&self) -> &Option<NumaNodes>;

    /// Register event notifier for reset of standard machine.
    ///
    /// # Arguments
    ///
    /// * `reset_req` - Eventfd of the reset request.
    /// * `clone_vm` - Reference of the StdMachine.
    fn register_reset_event(
        &self,
        reset_req: Arc<EventFd>,
        clone_vm: Arc<Mutex<StdMachine>>,
    ) -> MachineResult<()> {
        let reset_req_fd = reset_req.as_raw_fd();
        let reset_req_handler: Rc<NotifierCallback> = Rc::new(move |_, _| {
            read_fd(reset_req_fd);
            if let Err(e) = StdMachine::handle_reset_request(&clone_vm) {
                error!("Fail to reboot standard VM, {:?}", e);
            }

            None
        });
        let notifier = EventNotifier::new(
            NotifierOperation::AddShared,
            reset_req_fd,
            None,
            EventSet::IN,
            vec![reset_req_handler],
        );
        EventLoop::update_event(vec![notifier], None)
            .with_context(|| "Failed to register event notifier.")?;
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    fn register_acpi_shutdown_event(
        &self,
        shutdown_req: Arc<EventFd>,
        clone_vm: Arc<Mutex<StdMachine>>,
    ) -> MachineResult<()> {
        use util::loop_context::gen_delete_notifiers;

        let shutdown_req_fd = shutdown_req.as_raw_fd();
        let shutdown_req_handler: Rc<NotifierCallback> = Rc::new(move |_, _| {
            let _ret = shutdown_req.read().unwrap();
            StdMachine::handle_shutdown_request(&clone_vm);
            Some(gen_delete_notifiers(&[shutdown_req_fd]))
        });
        let notifier = EventNotifier::new(
            NotifierOperation::AddShared,
            shutdown_req_fd,
            None,
            EventSet::IN,
            vec![shutdown_req_handler],
        );
        EventLoop::update_event(vec![notifier], None)
            .with_context(|| "Failed to register event notifier.")?;
        Ok(())
    }
}

/// Trait that helps to build ACPI tables.
/// Standard machine struct should at least implement `build_dsdt_table`, `build_madt_table`
/// and `build_mcfg_table` function.
trait AcpiBuilder {
    /// Add ACPI table to the end of table loader, returns the offset of ACPI table in `acpi_data`.
    ///
    /// # Arguments
    ///
    /// `acpi_data` - Bytes streams that ACPI tables converts to.
    /// `loader` - ACPI table loader.
    /// `table` - ACPI table.
    fn add_table_to_loader(
        acpi_data: &Arc<Mutex<Vec<u8>>>,
        loader: &mut TableLoader,
        table: &AcpiTable,
    ) -> Result<u64> {
        let mut locked_acpi_data = acpi_data.lock().unwrap();
        let table_begin = locked_acpi_data.len() as u32;
        locked_acpi_data.extend(table.aml_bytes());
        let table_end = locked_acpi_data.len() as u32;
        // Drop the lock of acpi_data to avoid dead-lock when adding entry to
        // TableLoader, because TableLoader also needs to acquire this lock.
        drop(locked_acpi_data);

        loader.add_cksum_entry(
            ACPI_TABLE_FILE,
            table_begin + TABLE_CHECKSUM_OFFSET,
            table_begin,
            table_end - table_begin,
        )?;

        Ok(table_begin as u64)
    }

    /// Build ACPI DSDT table, returns the offset of ACPI DSDT table in `acpi_data`.
    ///
    /// # Arguments
    ///
    /// `acpi_data` - Bytes streams that ACPI tables converts to.
    /// `loader` - ACPI table loader.
    fn build_dsdt_table(
        &self,
        _acpi_data: &Arc<Mutex<Vec<u8>>>,
        _loader: &mut TableLoader,
    ) -> Result<u64> {
        bail!("Not implemented");
    }

    /// Build ACPI MADT table, returns the offset of ACPI MADT table in `acpi_data`.
    ///
    /// # Arguments
    ///
    /// `acpi_data` - Bytes streams that ACPI tables converts to.
    /// `loader` - ACPI table loader.
    fn build_madt_table(
        &self,
        _acpi_data: &Arc<Mutex<Vec<u8>>>,
        _loader: &mut TableLoader,
    ) -> Result<u64> {
        bail!("Not implemented");
    }

    /// Build ACPI GTDT table, returns the offset of ACPI GTDT table in `acpi_data`.
    ///
    /// # Arguments
    ///
    /// `acpi_data` - Bytes streams that ACPI tables converts to.
    /// `loader` - ACPI table loader.
    #[cfg(target_arch = "aarch64")]
    fn build_gtdt_table(
        &self,
        _acpi_data: &Arc<Mutex<Vec<u8>>>,
        _loader: &mut TableLoader,
    ) -> Result<u64>
    where
        Self: Sized,
    {
        Ok(0)
    }

    /// Build ACPI IORT table, returns the offset of ACPI IORT table in `acpi_data`.
    ///
    /// # Arguments
    ///
    /// `acpi_data` - Bytes streams that ACPI tables converts to.
    /// `loader` - ACPI table loader.
    #[cfg(target_arch = "aarch64")]
    fn build_iort_table(
        &self,
        _acpi_data: &Arc<Mutex<Vec<u8>>>,
        _loader: &mut TableLoader,
    ) -> Result<u64>
    where
        Self: Sized,
    {
        Ok(0)
    }

    /// Build ACPI SPCR table, returns the offset of ACPI SPCR table in `acpi_data`.
    ///
    /// # Arguments
    ///
    /// `acpi_data` - Bytes streams that ACPI tables converts to.
    /// `loader` - ACPI table loader.
    #[cfg(target_arch = "aarch64")]
    fn build_spcr_table(
        &self,
        _acpi_data: &Arc<Mutex<Vec<u8>>>,
        _loader: &mut TableLoader,
    ) -> Result<u64>
    where
        Self: Sized,
    {
        Ok(0)
    }

    /// Build ACPI PPTT table, returns the offset of ACPI PPTT table in `acpi_data`.
    ///
    /// # Arguments
    ///
    /// `acpi_data` - Bytes streams that ACPI tables converts to.
    /// `Loader` - ACPI table loader.
    #[cfg(target_arch = "aarch64")]
    fn build_pptt_table(
        &self,
        _acpi_data: &Arc<Mutex<Vec<u8>>>,
        _loader: &mut TableLoader,
    ) -> Result<u64>
    where
        Self: Sized,
    {
        Ok(0)
    }

    /// Build ACPI MCFG table, returns the offset of ACPI MCFG table in `acpi_data`.
    ///
    /// # Arguments
    ///
    /// `acpi_data` - Bytes streams that ACPI tables converts to.
    /// `loader` - ACPI table loader.
    fn build_mcfg_table(acpi_data: &Arc<Mutex<Vec<u8>>>, loader: &mut TableLoader) -> Result<u64>
    where
        Self: Sized,
    {
        let mut mcfg = AcpiTable::new(*b"MCFG", 1, *b"STRATO", *b"VIRTMCFG", 1);
        // Bits 20~28 (totally 9 bits) in PCIE ECAM represents bus number.
        let bus_number_mask = (1 << 9) - 1;
        let ecam_addr: u64;
        let max_nr_bus: u64;
        #[cfg(target_arch = "x86_64")]
        {
            ecam_addr = MEM_LAYOUT[LayoutEntryType::PcieEcam as usize].0;
            max_nr_bus = (MEM_LAYOUT[LayoutEntryType::PcieEcam as usize].1 >> 20) & bus_number_mask;
        }
        #[cfg(target_arch = "aarch64")]
        {
            ecam_addr = MEM_LAYOUT[LayoutEntryType::HighPcieEcam as usize].0;
            max_nr_bus =
                (MEM_LAYOUT[LayoutEntryType::HighPcieEcam as usize].1 >> 20) & bus_number_mask;
        }

        // Reserved
        mcfg.append_child(&[0_u8; 8]);
        // Base address of PCIE ECAM
        mcfg.append_child(ecam_addr.as_bytes());
        // PCI Segment Group Number
        mcfg.append_child(0_u16.as_bytes());
        // Start Bus Number and End Bus Number
        mcfg.append_child(&[0_u8, (max_nr_bus - 1) as u8]);
        // Reserved
        mcfg.append_child(&[0_u8; 4]);

        let mut acpi_data_locked = acpi_data.lock().unwrap();
        let mcfg_begin = acpi_data_locked.len() as u32;
        acpi_data_locked.extend(mcfg.aml_bytes());
        let mcfg_end = acpi_data_locked.len() as u32;
        drop(acpi_data_locked);

        loader.add_cksum_entry(
            ACPI_TABLE_FILE,
            mcfg_begin + TABLE_CHECKSUM_OFFSET,
            mcfg_begin,
            mcfg_end - mcfg_begin,
        )?;
        Ok(mcfg_begin as u64)
    }

    /// Build ACPI FADT table, returns the offset of ACPI FADT table in `acpi_data`.
    ///
    /// # Arguments
    ///
    /// `acpi_data` - Bytes streams that ACPI tables converts to.
    /// `loader` - ACPI table loader.
    /// `dsdt_addr` - Offset of ACPI DSDT table in `acpi_data`.
    fn build_fadt_table(
        acpi_data: &Arc<Mutex<Vec<u8>>>,
        loader: &mut TableLoader,
        dsdt_addr: u64,
    ) -> Result<u64>
    where
        Self: Sized,
    {
        let mut fadt = AcpiTable::new(*b"FACP", 6, *b"STRATO", *b"VIRTFACP", 1);

        fadt.set_table_len(208_usize);
        // PM1A_EVENT bit, offset is 56.
        #[cfg(target_arch = "x86_64")]
        fadt.set_field(56, 0x600);
        // PM1A_CONTROL bit, offset is 64.
        #[cfg(target_arch = "x86_64")]
        fadt.set_field(64, 0x604);
        // PM_TMR_BLK bit, offset is 76.
        #[cfg(target_arch = "x86_64")]
        fadt.set_field(76, 0x608);
        #[cfg(target_arch = "aarch64")]
        {
            // FADT flag: enable HW_REDUCED_ACPI bit on aarch64 plantform.
            fadt.set_field(112, 1 << 20 | 1 << 10 | 1 << 8);
            // ARM Boot Architecture Flags
            fadt.set_field(129, 0x3_u16);
        }
        // FADT minor revision
        fadt.set_field(131, 3);
        // X_PM_TMR_BLK bit, offset is 208.
        #[cfg(target_arch = "x86_64")]
        fadt.append_child(&AcpiGenericAddress::new_io_address(0x608_u32).aml_bytes());
        // FADT table size is fixed.
        fadt.set_table_len(276_usize);

        #[cfg(target_arch = "x86_64")]
        {
            // FADT flag: disable HW_REDUCED_ACPI bit on x86 plantform.
            fadt.set_field(112, 1 << 10 | 1 << 8);
            // Reset Register bit, offset is 116.
            fadt.set_field(116, 0x01_u8);
            fadt.set_field(117, 0x08_u8);
            fadt.set_field(120, RST_CTRL_OFFSET as u64);
            fadt.set_field(128, 0x0F_u8);
            // PM1a event register bit, offset is 148.
            fadt.set_field(148, 0x01_u8);
            fadt.set_field(149, 0x20_u8);
            fadt.set_field(152, PM_EVENT_OFFSET as u64);
            // PM1a control register bit, offset is 172.
            fadt.set_field(172, 0x01_u8);
            fadt.set_field(173, 0x10_u8);
            fadt.set_field(176, PM_CTRL_OFFSET as u64);
            // Sleep control register, offset is 244.
            fadt.set_field(244, 0x01_u8);
            fadt.set_field(245, 0x08_u8);
            fadt.set_field(248, SLEEP_CTRL_OFFSET as u64);
            // Sleep status tegister, offset is 256.
            fadt.set_field(256, 0x01_u8);
            fadt.set_field(257, 0x08_u8);
            fadt.set_field(260, SLEEP_CTRL_OFFSET as u64);
        }

        let mut locked_acpi_data = acpi_data.lock().unwrap();
        let fadt_begin = locked_acpi_data.len() as u32;
        locked_acpi_data.extend(fadt.aml_bytes());
        let fadt_end = locked_acpi_data.len() as u32;
        drop(locked_acpi_data);

        // xDSDT address field's offset in FADT.
        let xdsdt_offset = 140_u32;
        // Size of xDSDT address.
        let xdsdt_size = 8_u8;
        loader.add_pointer_entry(
            ACPI_TABLE_FILE,
            fadt_begin + xdsdt_offset,
            xdsdt_size,
            ACPI_TABLE_FILE,
            dsdt_addr as u32,
        )?;

        loader.add_cksum_entry(
            ACPI_TABLE_FILE,
            fadt_begin + TABLE_CHECKSUM_OFFSET,
            fadt_begin,
            fadt_end - fadt_begin,
        )?;

        Ok(fadt_begin as u64)
    }

    /// Build ACPI FACS table, returns the offset of ACPI FACS table in `acpi_data`.
    ///
    /// # Arguments
    ///
    /// `acpi_data` - Bytes streams that ACPI tables converts to.
    /// `loader` - ACPI table loader.
    #[cfg(target_arch = "x86_64")]
    fn build_facs_table(acpi_data: &Arc<Mutex<Vec<u8>>>, loader: &mut TableLoader) -> Result<u64>
    where
        Self: Sized,
    {
        let mut facs_data = vec![0_u8; 0x40];
        // FACS table signature.
        facs_data[0] = b'F';
        facs_data[1] = b'A';
        facs_data[2] = b'C';
        facs_data[3] = b'S';
        // FACS table length.
        facs_data[4] = 0x40;

        let mut locked_acpi_data = acpi_data.lock().unwrap();
        let facs_begin = locked_acpi_data.len() as u32;
        locked_acpi_data.extend(facs_data);
        let facs_end = locked_acpi_data.len() as u32;
        drop(locked_acpi_data);

        loader.add_cksum_entry(
            ACPI_TABLE_FILE,
            facs_begin + TABLE_CHECKSUM_OFFSET,
            facs_begin,
            facs_end - facs_begin,
        )?;

        Ok(facs_begin as u64)
    }

    /// Build ACPI SRAT CPU table.
    ///  # Arguments
    ///
    /// `proximity_domain` - The proximity domain.
    /// `node` - The NUMA node.
    /// `srat` - The SRAT table.
    fn build_srat_cpu(&self, proximity_domain: u32, node: &NumaNode, srat: &mut AcpiTable);

    /// Build ACPI SRAT memory table.
    ///  # Arguments
    ///
    /// `base_addr` - The base address of the memory range.
    /// `proximity_domain` - The proximity domain.
    /// `node` - The NUMA node.
    /// `srat` - The SRAT table.
    fn build_srat_mem(
        &self,
        base_addr: u64,
        proximity_domain: u32,
        node: &NumaNode,
        srat: &mut AcpiTable,
    ) -> u64;

    /// Build ACPI SRAT table, returns the offset of ACPI SRAT table in `acpi_data`.
    ///
    /// # Arguments
    ///
    /// `acpi_data` - Bytes streams that ACPI tables converts to.
    /// `loader` - ACPI table loader.
    fn build_srat_table(
        &self,
        acpi_data: &Arc<Mutex<Vec<u8>>>,
        loader: &mut TableLoader,
    ) -> Result<u64>;

    /// Build ACPI SLIT table, returns the offset of ACPI SLIT table in `acpi_data`.
    ///
    /// # Arguments
    ///
    /// `numa_nodes` - The information of NUMA nodes.
    /// `acpi_data` - Bytes streams that ACPI tables converts to.
    /// `loader` - ACPI table loader.
    fn build_slit_table(
        numa_nodes: &NumaNodes,
        acpi_data: &Arc<Mutex<Vec<u8>>>,
        loader: &mut TableLoader,
    ) -> Result<u64> {
        let mut slit = AcpiTable::new(*b"SLIT", 1, *b"STRATO", *b"VIRTSLIT", 1);
        slit.append_child((numa_nodes.len() as u64).as_bytes());

        let existing_nodes: Vec<u32> = numa_nodes.keys().cloned().collect();
        for (id, node) in numa_nodes.iter().enumerate() {
            let distances = &node.1.distances;
            for i in existing_nodes.iter() {
                let dist: u8 = if id as u32 == *i {
                    10
                } else if let Some(distance) = distances.get(i) {
                    *distance
                } else {
                    20
                };
                slit.append_child(dist.as_bytes());
            }
        }

        let slit_begin = StdMachine::add_table_to_loader(acpi_data, loader, &slit)
            .with_context(|| "Fail to add SLIT table to loader")?;
        Ok(slit_begin)
    }

    /// Build ACPI XSDT table, returns the offset of ACPI XSDT table in `acpi_data`.
    ///
    /// # Arguments
    ///
    /// `acpi_data` - Bytes streams that ACPI tables converts to.
    /// `loader` - ACPI table loader.
    /// `xsdt_entries` - Offset of table entries in `acpi_data`, such as FADT, MADT, MCFG table.
    fn build_xsdt_table(
        acpi_data: &Arc<Mutex<Vec<u8>>>,
        loader: &mut TableLoader,
        xsdt_entries: Vec<u64>,
    ) -> Result<u64>
    where
        Self: Sized,
    {
        let mut xsdt = AcpiTable::new(*b"XSDT", 1, *b"STRATO", *b"VIRTXSDT", 1);

        xsdt.set_table_len(xsdt.table_len() + size_of::<u64>() * xsdt_entries.len());

        let mut locked_acpi_data = acpi_data.lock().unwrap();
        let xsdt_begin = locked_acpi_data.len() as u32;
        locked_acpi_data.extend(xsdt.aml_bytes());
        let xsdt_end = locked_acpi_data.len() as u32;
        drop(locked_acpi_data);

        // Offset of table entries in XSDT.
        let mut entry_offset = 36_u32;
        // Size of each entry.
        let entry_size = size_of::<u64>() as u8;
        for entry in xsdt_entries {
            loader.add_pointer_entry(
                ACPI_TABLE_FILE,
                xsdt_begin + entry_offset,
                entry_size,
                ACPI_TABLE_FILE,
                entry as u32,
            )?;
            entry_offset += u32::from(entry_size);
        }

        loader.add_cksum_entry(
            ACPI_TABLE_FILE,
            xsdt_begin + TABLE_CHECKSUM_OFFSET,
            xsdt_begin,
            xsdt_end - xsdt_begin,
        )?;

        Ok(xsdt_begin as u64)
    }

    /// Build ACPI RSDP and add it to FwCfg as file-entry.
    ///
    /// # Arguments
    ///
    /// `loader` - ACPI table loader.
    /// `fw_cfg`: FwCfgOps trait object.
    /// `xsdt_addr` - Offset of ACPI XSDT table in `acpi_data`.
    fn build_rsdp(loader: &mut TableLoader, fw_cfg: &mut dyn FwCfgOps, xsdt_addr: u64) -> Result<()>
    where
        Self: Sized,
    {
        let rsdp = AcpiRsdp::new(*b"STRATO");
        let rsdp_data = Arc::new(Mutex::new(rsdp.aml_bytes().to_vec()));

        loader.add_alloc_entry(ACPI_RSDP_FILE, rsdp_data.clone(), 16, true)?;

        let xsdt_offset = 24_u32;
        let xsdt_size = 8_u8;
        loader.add_pointer_entry(
            ACPI_RSDP_FILE,
            xsdt_offset,
            xsdt_size,
            ACPI_TABLE_FILE,
            xsdt_addr as u32,
        )?;

        let cksum_offset = 8_u32;
        let exd_cksum_offset = 32_u32;
        loader.add_cksum_entry(ACPI_RSDP_FILE, cksum_offset, 0, 20)?;
        loader.add_cksum_entry(ACPI_RSDP_FILE, exd_cksum_offset, 0, 36)?;

        fw_cfg.add_file_entry(ACPI_RSDP_FILE, rsdp_data.lock().unwrap().to_vec())?;

        Ok(())
    }
}

fn get_device_bdf(bus: Option<String>, addr: Option<String>) -> Result<PciBdf> {
    let mut pci_bdf = PciBdf {
        bus: bus.unwrap_or_else(|| String::from("pcie.0")),
        addr: (0, 0),
    };
    let addr = addr.unwrap_or_else(|| String::from("0x0"));
    pci_bdf.addr = get_pci_df(&addr).with_context(|| "Failed to get device num or function num")?;
    Ok(pci_bdf)
}

impl StdMachine {
    fn plug_virtio_pci_blk(
        &mut self,
        pci_bdf: &PciBdf,
        args: &qmp_schema::DeviceAddArgument,
    ) -> Result<()> {
        let multifunction = args.multifunction.unwrap_or(false);
        let drive = args.drive.as_ref().with_context(|| "Drive not set")?;
        let queue_size = args.queue_size.unwrap_or(DEFAULT_VIRTQUEUE_SIZE);
        let vm_config = self.get_vm_config();
        let mut locked_vmconfig = vm_config.lock().unwrap();
        let nr_cpus = locked_vmconfig.machine_config.nr_cpus;
        let blk = if let Some(conf) = locked_vmconfig.drives.get(drive) {
            let dev = BlkDevConfig {
                id: args.id.clone(),
                path_on_host: conf.path_on_host.clone(),
                read_only: conf.read_only,
                direct: conf.direct,
                serial_num: args.serial_num.clone(),
                iothread: args.iothread.clone(),
                iops: conf.iops,
                queues: args.queues.unwrap_or_else(|| {
                    VirtioPciDevice::virtio_pci_auto_queues_num(0, nr_cpus, MAX_VIRTIO_QUEUE)
                }),
                boot_index: args.boot_index,
                chardev: None,
                socket_path: None,
                aio: conf.aio,
                queue_size,
                discard: conf.discard,
                write_zeroes: conf.write_zeroes,
            };
            dev.check()?;
            dev
        } else {
            bail!("Drive not found");
        };
        locked_vmconfig.add_blk_device_config(args);
        drop(locked_vmconfig);

        if let Some(bootindex) = args.boot_index {
            self.check_bootindex(bootindex)
                .with_context(|| "Fail to add virtio pci blk device for invalid bootindex")?;
        }

        let blk_id = blk.id.clone();
        let blk = Arc::new(Mutex::new(Block::new(blk, self.get_drive_files())));
        let pci_dev = self
            .add_virtio_pci_device(&args.id, pci_bdf, blk.clone(), multifunction, false)
            .with_context(|| "Failed to add virtio pci block device")?;

        if let Some(bootindex) = args.boot_index {
            if let Some(dev_path) = pci_dev.lock().unwrap().get_dev_path() {
                self.add_bootindex_devices(bootindex, &dev_path, &args.id);
            }
        }

        MigrationManager::register_device_instance(BlockState::descriptor(), blk, &blk_id);
        Ok(())
    }

    fn plug_virtio_pci_scsi(
        &mut self,
        pci_bdf: &PciBdf,
        args: &qmp_schema::DeviceAddArgument,
    ) -> Result<()> {
        let multifunction = args.multifunction.unwrap_or(false);
        let nr_cpus = self.get_vm_config().lock().unwrap().machine_config.nr_cpus;
        let queue_size = args.queue_size.unwrap_or(DEFAULT_VIRTQUEUE_SIZE);
        let dev_cfg = ScsiCntlrConfig {
            id: args.id.clone(),
            iothread: args.iothread.clone(),
            queues: args.queues.unwrap_or_else(|| {
                VirtioPciDevice::virtio_pci_auto_queues_num(0, nr_cpus, MAX_VIRTIO_QUEUE)
            }) as u32,
            boot_prefix: None,
            queue_size,
        };
        dev_cfg.check()?;

        let device = Arc::new(Mutex::new(ScsiCntlr::new(dev_cfg.clone())));

        let bus_name = format!("{}.0", dev_cfg.id);
        scsi_cntlr_create_scsi_bus(&bus_name, &device)?;

        let virtio_pci_dev = self
            .add_virtio_pci_device(&args.id, pci_bdf, device.clone(), multifunction, false)
            .with_context(|| "Failed to add virtio scsi controller")?;
        device.lock().unwrap().config.boot_prefix = virtio_pci_dev.lock().unwrap().get_dev_path();

        Ok(())
    }

    fn plug_vhost_user_blk_pci(
        &mut self,
        pci_bdf: &PciBdf,
        args: &qmp_schema::DeviceAddArgument,
    ) -> Result<()> {
        let multifunction = args.multifunction.unwrap_or(false);
        let vm_config = self.get_vm_config();
        let locked_vmconfig = vm_config.lock().unwrap();
        let chardev = args.chardev.as_ref().with_context(|| "Chardev not set")?;
        let queue_size = args.queue_size.unwrap_or(DEFAULT_VIRTQUEUE_SIZE);
        let socket_path = self
            .get_socket_path(&locked_vmconfig, chardev.to_string())
            .with_context(|| "Failed to get socket path")?;
        let nr_cpus = locked_vmconfig.machine_config.nr_cpus;
        let dev = BlkDevConfig {
            id: args.id.clone(),
            queues: args.queues.unwrap_or_else(|| {
                VirtioPciDevice::virtio_pci_auto_queues_num(0, nr_cpus, MAX_VIRTIO_QUEUE)
            }),
            boot_index: args.boot_index,
            chardev: Some(chardev.to_string()),
            socket_path,
            queue_size,
            ..BlkDevConfig::default()
        };

        dev.check()?;
        drop(locked_vmconfig);

        let blk = Arc::new(Mutex::new(VhostUser::Block::new(&dev, self.get_sys_mem())));
        self.add_virtio_pci_device(&args.id, pci_bdf, blk, multifunction, true)
            .with_context(|| "Failed to add vhost user blk pci device")?;

        Ok(())
    }

    fn get_socket_path(&self, vm_config: &VmConfig, chardev: String) -> Result<Option<String>> {
        let char_dev = vm_config
            .chardev
            .get(&chardev)
            .with_context(|| format!("Chardev: {:?} not found for character device", &chardev))?;

        let socket_path = match &char_dev.backend {
            ChardevType::Socket {
                path,
                server,
                nowait,
            } => {
                if *server || *nowait {
                    bail!(
                        "Argument \'server\' or \'nowait\' is not needed for chardev \'{}\'",
                        path
                    );
                }
                Some(path.clone())
            }
            _ => {
                bail!("Chardev {:?} backend should be socket type.", &chardev);
            }
        };

        Ok(socket_path)
    }

    fn plug_virtio_pci_net(
        &mut self,
        pci_bdf: &PciBdf,
        args: &qmp_schema::DeviceAddArgument,
    ) -> Result<()> {
        let multifunction = args.multifunction.unwrap_or(false);
        let netdev = args.netdev.as_ref().with_context(|| "Netdev not set")?;
        let queue_size = args.queue_size.unwrap_or(DEFAULT_VIRTQUEUE_SIZE);
        let vm_config = self.get_vm_config();
        let mut locked_vmconfig = vm_config.lock().unwrap();
        let dev = if let Some(conf) = locked_vmconfig.netdevs.get(netdev) {
            let mut socket_path: Option<String> = None;
            if let Some(chardev) = &conf.chardev {
                socket_path = self
                    .get_socket_path(&locked_vmconfig, (&chardev).to_string())
                    .with_context(|| "Failed to get socket path")?;
            }
            let dev = NetworkInterfaceConfig {
                id: args.id.clone(),
                host_dev_name: conf.ifname.clone(),
                mac: args.mac.clone(),
                tap_fds: conf.tap_fds.clone(),
                vhost_type: conf.vhost_type.clone(),
                vhost_fds: conf.vhost_fds.clone(),
                iothread: args.iothread.clone(),
                queues: conf.queues,
                mq: conf.queues > 2,
                socket_path,
                queue_size,
            };
            dev.check()?;
            dev
        } else {
            bail!("Netdev not found");
        };
        locked_vmconfig.add_net_device_config(args);
        drop(locked_vmconfig);

        if dev.vhost_type.is_some() {
            let net: Arc<Mutex<dyn VirtioDevice>> =
                if dev.vhost_type == Some(String::from("vhost-kernel")) {
                    Arc::new(Mutex::new(VhostKern::Net::new(&dev, self.get_sys_mem())))
                } else {
                    Arc::new(Mutex::new(VhostUser::Net::new(&dev, self.get_sys_mem())))
                };
            self.add_virtio_pci_device(&args.id, pci_bdf, net, multifunction, true)
                .with_context(|| "Failed to add vhost-kernel/vhost-user net device")?;
        } else {
            let net_id = dev.id.clone();
            let net = Arc::new(Mutex::new(virtio::Net::new(dev)));
            self.add_virtio_pci_device(&args.id, pci_bdf, net.clone(), multifunction, false)
                .with_context(|| "Failed to add virtio net device")?;
            MigrationManager::register_device_instance(VirtioNetState::descriptor(), net, &net_id);
        }

        Ok(())
    }

    #[cfg(not(target_env = "musl"))]
    fn plug_usb_device(&mut self, args: &qmp_schema::DeviceAddArgument) -> Result<()> {
        let driver = args.driver.as_str();
        let vm_config = self.get_vm_config();
        let mut locked_vmconfig = vm_config.lock().unwrap();
        let cfg_args = format!("id={}", args.id);
        match driver {
            "usb-kbd" => {
                self.add_usb_keyboard(&mut locked_vmconfig, &cfg_args)?;
            }
            "usb-tablet" => {
                self.add_usb_tablet(&mut locked_vmconfig, &cfg_args)?;
            }
            _ => {
                bail!("Invalid usb device driver '{}'", driver);
            }
        };

        Ok(())
    }

    #[cfg(not(target_env = "musl"))]
    fn handle_unplug_usb_request(&mut self, id: String) -> Result<()> {
        let vm_config = self.get_vm_config();
        let mut locked_vmconfig = vm_config.lock().unwrap();
        self.detach_usb_from_xhci_controller(&mut locked_vmconfig, id)?;

        Ok(())
    }

    fn plug_vfio_pci_device(
        &mut self,
        bdf: &PciBdf,
        args: &qmp_schema::DeviceAddArgument,
    ) -> Result<()> {
        if args.host.is_none() && args.sysfsdev.is_none() {
            bail!("Neither option \"host\" nor \"sysfsdev\" was not provided.");
        }
        if args.host.is_some() && args.sysfsdev.is_some() {
            bail!("Both option \"host\" and \"sysfsdev\" was provided.");
        }

        let host = args.host.as_ref().map_or("", String::as_str);
        let sysfsdev = args.sysfsdev.as_ref().map_or("", String::as_str);
        let multifunc = args.multifunction.unwrap_or(false);
        self.create_vfio_pci_device(&args.id, bdf, host, sysfsdev, multifunc)
            .with_context(|| "Failed to plug vfio-pci device.")?;

        Ok(())
    }
}

impl DeviceInterface for StdMachine {
    fn query_status(&self) -> Response {
        let vm_state = self.get_vm_state();
        let vmstate = vm_state.deref().0.lock().unwrap();
        let qmp_state = match *vmstate {
            KvmVmState::Running => qmp_schema::StatusInfo {
                singlestep: false,
                running: true,
                status: qmp_schema::RunState::running,
            },
            KvmVmState::Paused => qmp_schema::StatusInfo {
                singlestep: false,
                running: false,
                status: qmp_schema::RunState::paused,
            },
            _ => Default::default(),
        };

        Response::create_response(serde_json::to_value(&qmp_state).unwrap(), None)
    }

    fn query_cpus(&self) -> Response {
        let mut cpu_vec: Vec<serde_json::Value> = Vec::new();
        let cpu_topo = self.get_cpu_topo();
        let cpus = self.get_cpus();
        for cpu_index in 0..cpu_topo.max_cpus {
            if cpu_topo.get_mask(cpu_index as usize) == 1 {
                let thread_id = cpus[cpu_index as usize].tid();
                let cpu_instance = cpu_topo.get_topo_instance_for_qmp(cpu_index as usize);
                let cpu_common = qmp_schema::CpuInfoCommon {
                    current: true,
                    qom_path: String::from("/machine/unattached/device[")
                        + &cpu_index.to_string()
                        + "]",
                    halted: false,
                    props: Some(cpu_instance),
                    CPU: cpu_index as isize,
                    thread_id: thread_id as isize,
                };
                #[cfg(target_arch = "x86_64")]
                {
                    let cpu_info = qmp_schema::CpuInfo::x86 {
                        common: cpu_common,
                        x86: qmp_schema::CpuInfoX86 {},
                    };
                    cpu_vec.push(serde_json::to_value(cpu_info).unwrap());
                }
                #[cfg(target_arch = "aarch64")]
                {
                    let cpu_info = qmp_schema::CpuInfo::Arm {
                        common: cpu_common,
                        arm: qmp_schema::CpuInfoArm {},
                    };
                    cpu_vec.push(serde_json::to_value(cpu_info).unwrap());
                }
            }
        }
        Response::create_response(cpu_vec.into(), None)
    }

    fn query_hotpluggable_cpus(&self) -> Response {
        Response::create_empty_response()
    }

    fn balloon(&self, value: u64) -> Response {
        if qmp_balloon(value) {
            return Response::create_empty_response();
        }
        Response::create_error_response(
            qmp_schema::QmpErrorClass::DeviceNotActive(
                "No balloon device has been activated".to_string(),
            ),
            None,
        )
    }

    fn query_balloon(&self) -> Response {
        if let Some(actual) = qmp_query_balloon() {
            let ret = qmp_schema::BalloonInfo { actual };
            return Response::create_response(serde_json::to_value(&ret).unwrap(), None);
        }
        Response::create_error_response(
            qmp_schema::QmpErrorClass::DeviceNotActive(
                "No balloon device has been activated".to_string(),
            ),
            None,
        )
    }

    fn query_vnc(&self) -> Response {
        #[cfg(not(target_env = "musl"))]
        if let Some(vnc_info) = qmp_query_vnc() {
            return Response::create_response(serde_json::to_value(&vnc_info).unwrap(), None);
        }
        Response::create_error_response(
            qmp_schema::QmpErrorClass::GenericError(
                "The service of VNC is not supported".to_string(),
            ),
            None,
        )
    }

    fn device_add(&mut self, args: Box<qmp_schema::DeviceAddArgument>) -> Response {
        if let Err(e) = self.check_device_id_existed(&args.id) {
            return Response::create_error_response(
                qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                None,
            );
        }

        // Use args.bus.clone() and args.addr.clone() because args borrowed in the following process.
        let pci_bdf = match get_device_bdf(args.bus.clone(), args.addr.clone()) {
            Ok(bdf) => bdf,
            Err(e) => {
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                    None,
                )
            }
        };

        let driver = args.driver.as_str();
        match driver {
            "virtio-blk-pci" => {
                if let Err(e) = self.plug_virtio_pci_blk(&pci_bdf, args.as_ref()) {
                    error!("{:?}", e);
                    let err_str = format!("Failed to add virtio pci blk: {}", e);
                    return Response::create_error_response(
                        qmp_schema::QmpErrorClass::GenericError(err_str),
                        None,
                    );
                }
            }
            "virtio-scsi-pci" => {
                if let Err(e) = self.plug_virtio_pci_scsi(&pci_bdf, args.as_ref()) {
                    error!("{:?}", e);
                    let err_str = format!("Failed to add virtio scsi controller: {}", e);
                    return Response::create_error_response(
                        qmp_schema::QmpErrorClass::GenericError(err_str),
                        None,
                    );
                }
            }
            "vhost-user-blk-pci" => {
                if let Err(e) = self.plug_vhost_user_blk_pci(&pci_bdf, args.as_ref()) {
                    error!("{:?}", e);
                    let err_str = format!("Failed to add vhost user blk pci: {}", e);
                    return Response::create_error_response(
                        qmp_schema::QmpErrorClass::GenericError(err_str),
                        None,
                    );
                }
            }
            "virtio-net-pci" => {
                if let Err(e) = self.plug_virtio_pci_net(&pci_bdf, args.as_ref()) {
                    error!("{:?}", e);
                    let err_str = format!("Failed to add virtio pci net: {}", e);
                    return Response::create_error_response(
                        qmp_schema::QmpErrorClass::GenericError(err_str),
                        None,
                    );
                }
            }
            "vfio-pci" => {
                if let Err(e) = self.plug_vfio_pci_device(&pci_bdf, args.as_ref()) {
                    error!("{:?}", e);
                    return Response::create_error_response(
                        qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                        None,
                    );
                }
            }
            #[cfg(not(target_env = "musl"))]
            "usb-kbd" | "usb-tablet" => {
                if let Err(e) = self.plug_usb_device(args.as_ref()) {
                    error!("{:?}", e);
                    return Response::create_error_response(
                        qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                        None,
                    );
                }
                return Response::create_empty_response();
            }
            _ => {
                let err_str = format!("Failed to add device: Driver {} is not support", driver);
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(err_str),
                    None,
                );
            }
        }

        // It's safe to call get_pci_host().unwrap() because it has been checked before.
        let locked_pci_host = self.get_pci_host().unwrap().lock().unwrap();
        if let Some((bus, dev)) = PciBus::find_attached_bus(&locked_pci_host.root_bus, &args.id) {
            match handle_plug(&bus, &dev) {
                Ok(()) => Response::create_empty_response(),
                Err(e) => {
                    if let Err(e) = PciBus::detach_device(&bus, &dev) {
                        error!("{:?}", e);
                        error!("Failed to detach device");
                    }
                    let err_str = format!("Failed to plug device: {}", e);
                    Response::create_error_response(
                        qmp_schema::QmpErrorClass::GenericError(err_str),
                        None,
                    )
                }
            }
        } else {
            Response::create_error_response(
                qmp_schema::QmpErrorClass::GenericError(
                    "Failed to add device: Bus not found".to_string(),
                ),
                None,
            )
        }
    }

    fn device_del(&mut self, device_id: String) -> Response {
        let pci_host = match self.get_pci_host() {
            Ok(host) => host,
            Err(e) => {
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                    None,
                )
            }
        };

        let locked_pci_host = pci_host.lock().unwrap();
        if let Some((bus, dev)) = PciBus::find_attached_bus(&locked_pci_host.root_bus, &device_id) {
            return match handle_unplug_pci_request(&bus, &dev) {
                Ok(()) => {
                    let locked_dev = dev.lock().unwrap();
                    let dev_id = locked_dev.name();
                    drop(locked_pci_host);
                    self.del_bootindex_devices(&dev_id);
                    let vm_config = self.get_vm_config();
                    let mut locked_config = vm_config.lock().unwrap();
                    locked_config.del_device_by_id(device_id);
                    drop(locked_config);
                    Response::create_empty_response()
                }
                Err(e) => Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                    None,
                ),
            };
        }
        drop(locked_pci_host);

        // The device is not a pci device, assume it is a usb device.
        #[cfg(not(target_env = "musl"))]
        return match self.handle_unplug_usb_request(device_id) {
            Ok(()) => Response::create_empty_response(),
            Err(e) => Response::create_error_response(
                qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                None,
            ),
        };

        #[cfg(target_env = "musl")]
        {
            let err_str = format!("Failed to remove device: id {} not found", &device_id);
            Response::create_error_response(qmp_schema::QmpErrorClass::GenericError(err_str), None)
        }
    }

    fn blockdev_add(&self, args: Box<qmp_schema::BlockDevAddArgument>) -> Response {
        let mut config = DriveConfig {
            id: args.node_name,
            path_on_host: args.file.filename.clone(),
            read_only: args.read_only.unwrap_or(false),
            direct: true,
            iops: args.iops,
            // TODO Add aio option by qmp, now we set it based on "direct".
            aio: AioEngine::Native,
            media: "disk".to_string(),
            discard: false,
            write_zeroes: WriteZeroesState::Off,
        };
        if args.cache.is_some() && !args.cache.unwrap().direct.unwrap_or(true) {
            config.direct = false;
            config.aio = AioEngine::Off;
        }
        if let Some(discard) = args.discard {
            let ret = discard.as_str().parse::<ExBool>();
            if ret.is_err() {
                let err_msg = format!(
                    "Invalid discard argument '{}', expected 'unwrap' or 'ignore'",
                    discard
                );
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(err_msg),
                    None,
                );
            }
            config.discard = ret.unwrap().into();
        }
        if let Some(detect_zeroes) = args.detect_zeroes {
            let state = detect_zeroes.as_str().parse::<WriteZeroesState>();
            if state.is_err() {
                let err_msg = format!(
                    "Invalid write-zeroes argument '{}', expected 'on | off | unmap'",
                    detect_zeroes
                );
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(err_msg),
                    None,
                );
            }
            config.write_zeroes = state.unwrap();
        }
        if let Err(e) = config.check() {
            error!("{:?}", e);
            return Response::create_error_response(
                qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                None,
            );
        }
        // Check whether path is valid after configuration check
        if let Err(e) = config.check_path() {
            error!("{:?}", e);
            return Response::create_error_response(
                qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                None,
            );
        }
        // Register drive backend file for hotplug drive.
        if let Err(e) =
            self.register_drive_file(&args.file.filename, config.read_only, config.direct)
        {
            error!("{:?}", e);
            return Response::create_error_response(
                qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                None,
            );
        }
        match self
            .get_vm_config()
            .lock()
            .unwrap()
            .add_drive_with_config(config)
        {
            Ok(()) => Response::create_empty_response(),
            Err(e) => {
                error!("{:?}", e);
                // It's safe to unwrap as the path has been registered.
                self.unregister_drive_file(&args.file.filename).unwrap();
                Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                    None,
                )
            }
        }
    }

    fn blockdev_del(&self, node_name: String) -> Response {
        match self
            .get_vm_config()
            .lock()
            .unwrap()
            .del_drive_by_id(&node_name)
        {
            Ok(path) => {
                // It's safe to unwrap as the path has been registered.
                self.unregister_drive_file(&path).unwrap();
                Response::create_empty_response()
            }
            Err(e) => Response::create_error_response(
                qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                None,
            ),
        }
    }

    fn chardev_add(&mut self, args: qmp_schema::CharDevAddArgument) -> Response {
        let config = match get_chardev_config(args) {
            Ok(conf) => conf,
            Err(e) => {
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                    None,
                );
            }
        };

        if let Err(e) = config.check() {
            return Response::create_error_response(
                qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                None,
            );
        }

        match self
            .get_vm_config()
            .lock()
            .unwrap()
            .add_chardev_with_config(config)
        {
            Ok(()) => Response::create_empty_response(),
            Err(e) => Response::create_error_response(
                qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                None,
            ),
        }
    }

    fn chardev_remove(&mut self, id: String) -> Response {
        match self.get_vm_config().lock().unwrap().del_chardev_by_id(&id) {
            Ok(()) => Response::create_empty_response(),
            Err(e) => Response::create_error_response(
                qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                None,
            ),
        }
    }

    fn netdev_add(&mut self, args: Box<qmp_schema::NetDevAddArgument>) -> Response {
        let config = match get_netdev_config(args) {
            Ok(conf) => conf,
            Err(e) => {
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                    None,
                );
            }
        };

        match self
            .get_vm_config()
            .lock()
            .unwrap()
            .add_netdev_with_config(config)
        {
            Ok(()) => Response::create_empty_response(),
            Err(e) => Response::create_error_response(
                qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                None,
            ),
        }
    }

    fn netdev_del(&mut self, id: String) -> Response {
        match self.get_vm_config().lock().unwrap().del_netdev_by_id(&id) {
            Ok(()) => Response::create_empty_response(),
            Err(e) => Response::create_error_response(
                qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                None,
            ),
        }
    }

    fn getfd(&self, fd_name: String, if_fd: Option<RawFd>) -> Response {
        if let Some(fd) = if_fd {
            QmpChannel::set_fd(fd_name, fd);
            Response::create_empty_response()
        } else {
            let err_resp =
                qmp_schema::QmpErrorClass::GenericError("Invalid SCM message".to_string());
            Response::create_error_response(err_resp, None)
        }
    }

    fn update_region(&mut self, args: UpdateRegionArgument) -> Response {
        #[derive(Default)]
        struct DummyDevice {
            head: u64,
        }

        impl DummyDevice {
            fn read(&mut self, data: &mut [u8], _base: GuestAddress, _offset: u64) -> bool {
                if data.len() != std::mem::size_of::<u64>() {
                    return false;
                }

                for (i, data) in data.iter_mut().enumerate().take(std::mem::size_of::<u64>()) {
                    *data = (self.head >> (8 * i)) as u8;
                }
                true
            }

            fn write(&mut self, data: &[u8], _addr: GuestAddress, _offset: u64) -> bool {
                if data.len() != std::mem::size_of::<u64>() {
                    return false;
                }

                let ptr: *const u8 = data.as_ptr();
                let ptr: *const u64 = ptr as *const u64;
                self.head = unsafe { *ptr } * 2;
                true
            }
        }

        let dummy_dev = Arc::new(Mutex::new(DummyDevice::default()));
        let dummy_dev_clone = dummy_dev.clone();
        let read_ops = move |data: &mut [u8], addr: GuestAddress, offset: u64| -> bool {
            let mut device_locked = dummy_dev_clone.lock().unwrap();
            device_locked.read(data, addr, offset)
        };
        let dummy_dev_clone = dummy_dev;
        let write_ops = move |data: &[u8], addr: GuestAddress, offset: u64| -> bool {
            let mut device_locked = dummy_dev_clone.lock().unwrap();
            device_locked.write(data, addr, offset)
        };

        let dummy_dev_ops = RegionOps {
            read: Arc::new(read_ops),
            write: Arc::new(write_ops),
        };

        let mut fd = None;
        if args.region_type.eq("rom_device_region") || args.region_type.eq("ram_device_region") {
            if let Some(file_name) = args.device_fd_path {
                fd = Some(
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&file_name)
                        .unwrap(),
                );
            }
        }

        let region;
        match args.region_type.as_str() {
            "io_region" => {
                region = Region::init_io_region(args.size, dummy_dev_ops);
                if args.ioeventfd.is_some() && args.ioeventfd.unwrap() {
                    let ioeventfds = vec![RegionIoEventFd {
                        fd: Arc::new(EventFd::new(libc::EFD_NONBLOCK).unwrap()),
                        addr_range: AddressRange::from((
                            0,
                            args.ioeventfd_size.unwrap_or_default(),
                        )),
                        data_match: args.ioeventfd_data.is_some(),
                        data: args.ioeventfd_data.unwrap_or_default(),
                    }];
                    region.set_ioeventfds(&ioeventfds);
                }
            }
            "rom_device_region" => {
                region = Region::init_rom_device_region(
                    Arc::new(
                        HostMemMapping::new(
                            GuestAddress(args.offset),
                            None,
                            args.size,
                            fd.map(FileBackend::new_common),
                            false,
                            true,
                            true,
                        )
                        .unwrap(),
                    ),
                    dummy_dev_ops,
                );
            }
            "ram_device_region" => {
                region = Region::init_ram_device_region(Arc::new(
                    HostMemMapping::new(
                        GuestAddress(args.offset),
                        None,
                        args.size,
                        fd.map(FileBackend::new_common),
                        false,
                        true,
                        false,
                    )
                    .unwrap(),
                ));
            }
            _ => {
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError("invalid rergion_type".to_string()),
                    None,
                );
            }
        };

        region.set_priority(args.priority as i32);
        if let Some(read_only) = args.romd {
            if region.set_rom_device_romd(read_only).is_err() {
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(
                        "set_rom_device_romd failed".to_string(),
                    ),
                    None,
                );
            }
        }

        let sys_mem = self.get_sys_mem();
        match args.update_type.as_str() {
            "add" => {
                if sys_mem.root().add_subregion(region, args.offset).is_err() {
                    return Response::create_error_response(
                        qmp_schema::QmpErrorClass::GenericError("add subregion failed".to_string()),
                        None,
                    );
                }
            }
            "delete" => {
                region.set_offset(GuestAddress(args.offset));
                if sys_mem.root().delete_subregion(&region).is_err() {
                    return Response::create_error_response(
                        qmp_schema::QmpErrorClass::GenericError(
                            "delete subregion failed".to_string(),
                        ),
                        None,
                    );
                }
            }
            _ => {
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError("invalid update_type".to_string()),
                    None,
                )
            }
        };

        Response::create_empty_response()
    }

    #[cfg(not(target_env = "musl"))]
    fn input_event(&self, key: String, value: String) -> Response {
        match send_input_event(key, value) {
            Ok(()) => Response::create_empty_response(),
            Err(e) => Response::create_error_response(
                qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                None,
            ),
        }
    }

    fn human_monitor_command(&self, args: qmp_schema::HumanMonitorCmdArgument) -> Response {
        let cmd_args: Vec<&str> = args.command_line.split(' ').collect();
        match cmd_args[0] {
            "drive_add" => {
                // The drive_add command has three arguments splited by space:
                // "drive_add dummy file=/path/to/file,format=raw,if=none,id=drive-id..."
                // The 'dummy' here is a placeholder for pci address which is not needed for drive.
                if cmd_args.len() != 3 {
                    return Response::create_error_response(
                        qmp_schema::QmpErrorClass::GenericError(
                            "Invalid number of arguments".to_string(),
                        ),
                        None,
                    );
                }
                let drive_cfg = match self
                    .get_vm_config()
                    .lock()
                    .unwrap()
                    .add_block_drive(cmd_args[2])
                {
                    Ok(cfg) => cfg,
                    Err(ref e) => {
                        return Response::create_error_response(
                            qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                            None,
                        );
                    }
                };
                if let Err(e) = self.register_drive_file(
                    &drive_cfg.path_on_host,
                    drive_cfg.read_only,
                    drive_cfg.direct,
                ) {
                    error!("{:?}", e);
                    return Response::create_error_response(
                        qmp_schema::QmpErrorClass::GenericError(e.to_string()),
                        None,
                    );
                }
            }
            "drive_del" => {
                // The drive_del command has two arguments splited by space:
                // "drive_del drive-id"
                if cmd_args.len() != 2 {
                    return Response::create_error_response(
                        qmp_schema::QmpErrorClass::GenericError(
                            "Invalid number of arguments".to_string(),
                        ),
                        None,
                    );
                }
                return self.blockdev_del(cmd_args[1].to_string());
            }
            _ => {
                return Response::create_error_response(
                    qmp_schema::QmpErrorClass::GenericError(format!(
                        "Unsupported command: {}",
                        cmd_args[0]
                    )),
                    None,
                );
            }
        }
        Response::create_empty_response()
    }
}

#[cfg(not(target_env = "musl"))]
fn send_input_event(key: String, value: String) -> Result<()> {
    match key.as_str() {
        "keyboard" => {
            let vec: Vec<&str> = value.split(',').collect();
            if vec.len() != 2 {
                bail!("Invalid keyboard format: {}", value);
            }
            let keycode = vec[0].parse::<u16>()?;
            let down = vec[1].parse::<u8>()? == 1;
            key_event(keycode, down)?;
        }
        "pointer" => {
            let vec: Vec<&str> = value.split(',').collect();
            if vec.len() != 3 {
                bail!("Invalid pointer format: {}", value);
            }
            let x = vec[0].parse::<u32>()?;
            let y = vec[1].parse::<u32>()?;
            let btn = vec[2].parse::<u32>()?;
            point_event(btn, x, y)?;
        }
        _ => {
            bail!("Invalid input type: {}", key);
        }
    };
    Ok(())
}
