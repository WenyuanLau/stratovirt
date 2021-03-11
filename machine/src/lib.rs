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
#[macro_use]
extern crate machine_manager;

pub mod errors {
    error_chain! {
        links {
            AddressSpace(address_space::errors::Error, address_space::errors::ErrorKind);
            IntCtrl(devices::IntCtrlErrs::Error, devices::IntCtrlErrs::ErrorKind) #[cfg(target_arch = "aarch64")];
            Legacy(devices::LegacyErrs::Error, devices::LegacyErrs::ErrorKind);
            MicroVm(super::micro_vm::errors::Error, super::micro_vm::errors::ErrorKind);
            StdVm(super::standard_vm::errors::Error, super::standard_vm::errors::ErrorKind);
            Util(util::errors::Error, util::errors::ErrorKind);
            Virtio(virtio::errors::Error, virtio::errors::ErrorKind);
        }

        foreign_links {
            KvmIoctl(kvm_ioctls::Error);
        }

        errors {
            AddDevErr(dev: String) {
                display("Failed to add {} device.", dev)
            }
            LoadKernErr {
                display("Failed to load kernel.")
            }
            CrtMemSpaceErr {
                display("Failed to create memory address space")
            }
            CrtIoSpaceErr {
                display("Failed to create I/O address space")
            }
            RegMemRegionErr(base: u64, size: u64) {
                display("Failed to register region in memory space: base={},size={}", base, size)
            }
            InitPwrBtnErr {
                display("Failed to init power button.")
            }
            RlzVirtioMmioErr {
                display("Failed to realize virtio mmio.")
            }
            #[cfg(target_arch = "x86_64")]
            CrtIrqchipErr {
                display("Failed to create irq chip.")
            }
            #[cfg(target_arch = "x86_64")]
            SetTssErr {
                display("Failed to set tss address.")
            }
            #[cfg(target_arch = "x86_64")]
            CrtPitErr {
                display("Failed to create PIT.")
            }
            #[cfg(target_arch = "aarch64")]
            GenFdtErr {
                display("Failed to generate FDT.")
            }
            #[cfg(target_arch = "aarch64")]
            WrtFdtErr(addr: u64, size: usize) {
                display("Failed to write FDT: addr={}, size={}", addr, size)
            }
            RegNotifierErr {
                display("Failed to register event notifier.")
            }
            StartVcpuErr(id: u8) {
                display("Failed to run vcpu{}.", id)
            }
            PauseVcpuErr(id: u8) {
                display("Failed to pause vcpu{}.", id)
            }
            ResumeVcpuErr(id: u8) {
                display("Failed to resume vcpu{}.", id)
            }
            DestroyVcpuErr(id: u8) {
                display("Failed to destroy vcpu{}.", id)
            }
        }
    }
}

mod micro_vm;
mod standard_vm;

pub use micro_vm::LightMachine;

use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};

#[cfg(target_arch = "x86_64")]
use address_space::KvmIoListener;
use address_space::{create_host_mmaps, AddressSpace, KvmMemoryListener, Region};
use cpu::{ArchCPU, CPUBootConfig, CPUInterface, CPU};
use kvm_ioctls::{Kvm, VcpuFd, VmFd};
use machine_manager::config::{
    BalloonConfig, ConsoleConfig, DriveConfig, MachineMemConfig, NetworkInterfaceConfig, RngConfig,
    SerialConfig, VmConfig, VsockConfig,
};
use machine_manager::event_loop::EventLoop;
use machine_manager::machine::MachineInterface;
use util::loop_context::{EventNotifier, NotifierCallback, NotifierOperation};
use util::seccomp::{BpfRule, SeccompOpt, SyscallFilter};
use virtio::balloon_allow_list;
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::eventfd::EventFd;

use errors::{ErrorKind, Result, ResultExt};

pub trait MachineOps {
    /// Calculate the ranges of memory according to architecture.
    ///
    /// # Arguments
    ///
    /// * `mem_size` - memory size of VM.
    ///
    /// # Returns
    ///
    /// A array of ranges, it's element represents (start_addr, size).
    /// On x86_64, there is a gap ranged from (4G - 768M) to 4G, which will be skipped.
    fn arch_ram_ranges(&self, mem_size: u64) -> Vec<(u64, u64)>;

    fn load_boot_source(&self) -> Result<CPUBootConfig>;

    /// Init I/O & memory address space and mmap guest memory.
    ///
    /// # Arguments
    ///
    /// * `fds` - File descriptors obtained by opening KVM module and creating new VM.
    /// * `mem_config` - Memory setting.
    /// * `sys_io` - IO address space required for x86_64.
    /// * `sys_mem` - Memory address space.
    fn init_memory(
        &self,
        fds: (Kvm, &Arc<VmFd>),
        mem_config: &MachineMemConfig,
        #[cfg(target_arch = "x86_64")] sys_io: &Arc<AddressSpace>,
        sys_mem: &Arc<AddressSpace>,
    ) -> Result<()> {
        let kvm_fd = fds.0;
        let vm_fd = fds.1;
        sys_mem
            .register_listener(Box::new(KvmMemoryListener::new(
                kvm_fd.get_nr_memslots() as u32,
                vm_fd.clone(),
            )))
            .chain_err(|| "Failed to register KVM listener for memory space.")?;
        #[cfg(target_arch = "x86_64")]
        sys_io
            .register_listener(Box::new(KvmIoListener::new(vm_fd.clone())))
            .chain_err(|| "Failed to register KVM listener for I/O space.")?;

        // Init guest-memory
        // Define ram-region ranges according to architectures
        let ram_ranges = self.arch_ram_ranges(mem_config.mem_size);
        let mem_mappings = create_host_mmaps(&ram_ranges, &mem_config)
            .chain_err(|| "Failed to mmap guest ram.")?;
        for mmap in mem_mappings.iter() {
            let base = mmap.start_address().raw_value();
            let size = mmap.size();
            sys_mem
                .root()
                .add_subregion(Region::init_ram_region(mmap.clone()), base)
                .chain_err(|| ErrorKind::RegMemRegionErr(base, size))?;
        }

        Ok(())
    }

    /// Init vcpu register with boot message.
    ///
    /// # Arguments
    ///
    /// * `vm` - `MachineInterface` to obtain functions cpu can use.
    /// * `nr_cpus` - The number of vcpus.
    /// * `fds` - File descriptors obtained by creating new VM and creating new Vcpu in KVM.
    /// * `boot_cfg` - Boot message generated by reading boot source to guest memory.
    fn init_vcpu(
        vm: Arc<Mutex<dyn MachineInterface + Send + Sync>>,
        nr_cpus: u8,
        fds: (&Arc<VmFd>, &[Arc<VcpuFd>]),
        boot_cfg: &CPUBootConfig,
    ) -> Result<Vec<Arc<CPU>>> {
        let mut cpus = Vec::<Arc<CPU>>::new();

        for vcpu_id in 0..nr_cpus {
            #[cfg(target_arch = "aarch64")]
            let arch_cpu = ArchCPU::new(u32::from(vcpu_id));
            #[cfg(target_arch = "x86_64")]
            let arch_cpu = ArchCPU::new(u32::from(vcpu_id), u32::from(nr_cpus));

            let cpu = CPU::new(
                fds.1[vcpu_id as usize].clone(),
                vcpu_id,
                Arc::new(Mutex::new(arch_cpu)),
                vm.clone(),
            );
            cpus.push(Arc::new(cpu));
        }

        for cpu_index in 0..nr_cpus as usize {
            cpus[cpu_index as usize]
                .realize(fds.0, boot_cfg)
                .chain_err(|| {
                    format!(
                        "Failed to realize arch cpu register in CPU {}/KVM",
                        cpu_index
                    )
                })?;
        }

        Ok(cpus)
    }

    /// Add interrupt controller.
    ///
    /// # Arguments
    ///
    /// * `vm_fd` - File descriptors obtained creating new VM.
    /// * `vcpu_count` - The number of vcpu.
    fn init_interrupt_controller(&mut self, vm_fd: &Arc<VmFd>, _vcpu_count: u64) -> Result<()>;

    /// Add RTC device.
    ///
    /// # Arguments
    ///
    /// * `vm_fd` - File descriptor of VM.
    #[cfg(target_arch = "aarch64")]
    fn add_rtc_device(&mut self, vm_fd: &Arc<VmFd>) -> Result<()>;

    /// Add serial device.
    ///
    /// # Arguments
    ///
    /// * `config` - Device configuration.
    /// * `vm_fd` - File descriptor of VM.
    fn add_serial_device(&mut self, config: &SerialConfig, vm_fd: &Arc<VmFd>) -> Result<()>;

    /// Add block device.
    ///
    /// # Arguments
    ///
    /// * `config` - Device configuration.
    fn add_block_device(&mut self, config: &DriveConfig) -> Result<()>;

    /// Add vsock device.
    ///
    /// # Arguments
    ///
    /// * `config` - Device configuration.
    /// * `vm_fd` - File descriptor of VM.
    fn add_vsock_device(&mut self, config: &VsockConfig, vm_fd: &Arc<VmFd>) -> Result<()>;

    /// Add net device.
    ///
    /// # Arguments
    ///
    /// * `config` - Device configuration.
    /// * `vm_fd` - File descriptor of VM.
    fn add_net_device(&mut self, config: &NetworkInterfaceConfig, vm_fd: &Arc<VmFd>) -> Result<()>;

    /// Add console device.
    ///
    /// # Arguments
    ///
    /// * `config` - Device configuration.
    /// * `vm_fd` - File descriptor of VM.
    fn add_console_device(&mut self, config: &ConsoleConfig, vm_fd: &Arc<VmFd>) -> Result<()>;

    /// Add memory balloon device.
    ///
    /// # Arguments
    ///
    /// * `config` - Device configuration.
    /// * `vm_fd` - File descriptor of VM.
    fn add_balloon_device(&mut self, config: &BalloonConfig, vm_fd: &Arc<VmFd>) -> Result<()>;

    /// Add virtio-rng device.
    ///
    /// # Arguments
    ///
    /// * `config` - Device configuration.
    /// * `vm_fd` - File descriptor of VM.
    fn add_rng_device(&mut self, _config: &RngConfig, _vm_fd: &Arc<VmFd>) -> Result<()> {
        Ok(())
    }

    /// Add peripheral devices.
    ///
    /// # Arguments
    ///
    /// * `vm_config` - VM Configuration.
    /// * `vm_fd` - File descriptor of VM.
    fn add_devices(&mut self, vm_config: &VmConfig, vm_fd: &Arc<VmFd>) -> Result<()>;

    /// Return the syscall whitelist for seccomp.
    fn syscall_whitelist(&self) -> Vec<BpfRule>;

    /// Register seccomp rules in syscall whitelist to seccomp.
    fn register_seccomp(&self, balloon_enable: bool) -> Result<()> {
        let mut seccomp_filter = SyscallFilter::new(SeccompOpt::Trap);
        let mut bpf_rules = self.syscall_whitelist();
        if balloon_enable {
            balloon_allow_list(&mut bpf_rules);
        }

        for bpf_rule in &mut bpf_rules {
            seccomp_filter.push(bpf_rule);
        }
        seccomp_filter
            .realize()
            .chain_err(|| "Failed to init seccomp filter.")?;
        Ok(())
    }

    /// Register event notifier for power button of mainboard.
    ///
    /// # Arguments
    ///
    /// * `power_button` - Eventfd of the power button.
    fn register_power_event(&self, power_button: &EventFd) -> Result<()> {
        let power_button = power_button.try_clone().unwrap();
        let button_fd = power_button.as_raw_fd();
        let power_button_handler: Arc<Mutex<Box<NotifierCallback>>> =
            Arc::new(Mutex::new(Box::new(move |_, _| {
                let _ret = power_button.read().unwrap();
                None
            })));
        let notifier = EventNotifier::new(
            NotifierOperation::AddShared,
            button_fd,
            None,
            EventSet::IN,
            vec![power_button_handler],
        );

        EventLoop::update_event(vec![notifier], None).chain_err(|| ErrorKind::RegNotifierErr)?;
        Ok(())
    }

    /// Realize the machine.
    ///
    /// # Arguments
    ///
    /// * `vm` - The machine structure.
    /// * `vm_config` - VM configuration.
    /// * `fds` - File descriptors obtained by opening KVM module and creating a new VM.
    fn realize(vm: &Arc<Mutex<Self>>, vm_config: &VmConfig, fds: (Kvm, &Arc<VmFd>)) -> Result<()>;
}
