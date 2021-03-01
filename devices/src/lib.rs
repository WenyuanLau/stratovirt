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

//! Interfaces for simulating various devices.
//!
//! This crate simulates:
//! - interrupt controller (aarch64)
//! - legacy devices, such as serial devices

#[macro_use]
extern crate log;
#[macro_use]
extern crate error_chain;

mod interrupt_controller;
mod legacy;

pub mod errors {
    error_chain! {
        errors {
            #[cfg(target_arch = "aarch64")]
            InvalidConfig(err_info: String) {
                display("Invalid GIC config: {}.", err_info)
            }
            #[cfg(target_arch = "aarch64")]
            CreateKvmDevice(err: kvm_ioctls::Error) {
                display("Failed to create KVM device: {:#?}.", err)
            }
        }
    }
}

#[cfg(target_arch = "aarch64")]
pub use interrupt_controller::{InterruptController, InterruptControllerConfig};
pub use legacy::Serial;
#[cfg(target_arch = "aarch64")]
pub use legacy::PL031;
