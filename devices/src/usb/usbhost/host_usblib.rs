// Copyright (c) 2023 Huawei Technologies Co.,Ltd. All rights reserved.
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

use std::{
    rc::Rc,
    sync::{Arc, Mutex},
};

use libc::{c_int, EPOLLIN, EPOLLOUT};
use libusb1_sys::{
    constants::{
        LIBUSB_ERROR_ACCESS, LIBUSB_ERROR_BUSY, LIBUSB_ERROR_INTERRUPTED,
        LIBUSB_ERROR_INVALID_PARAM, LIBUSB_ERROR_IO, LIBUSB_ERROR_NOT_FOUND,
        LIBUSB_ERROR_NOT_SUPPORTED, LIBUSB_ERROR_NO_DEVICE, LIBUSB_ERROR_NO_MEM,
        LIBUSB_ERROR_OVERFLOW, LIBUSB_ERROR_PIPE, LIBUSB_ERROR_TIMEOUT, LIBUSB_TRANSFER_CANCELLED,
        LIBUSB_TRANSFER_COMPLETED, LIBUSB_TRANSFER_ERROR, LIBUSB_TRANSFER_NO_DEVICE,
        LIBUSB_TRANSFER_STALL, LIBUSB_TRANSFER_TIMED_OUT,
    },
    libusb_get_pollfds, libusb_pollfd, libusb_transfer,
};
use log::error;
use rusb::{Context, DeviceHandle, Error, Result, UsbContext};
use vmm_sys_util::epoll::EventSet;

use super::{UsbHost, UsbHostRequest};
use crate::usb::{UsbPacketStatus, USB_TOKEN_IN};
use util::loop_context::{EventNotifier, NotifierCallback, NotifierOperation};

const BULK_TIMEOUT: u32 = 0;
const INTERRUPT_TIMEOUT: u32 = 0;

fn from_libusb(err: i32) -> Error {
    match err {
        LIBUSB_ERROR_IO => Error::Io,
        LIBUSB_ERROR_INVALID_PARAM => Error::InvalidParam,
        LIBUSB_ERROR_ACCESS => Error::Access,
        LIBUSB_ERROR_NO_DEVICE => Error::NoDevice,
        LIBUSB_ERROR_NOT_FOUND => Error::NotFound,
        LIBUSB_ERROR_BUSY => Error::Busy,
        LIBUSB_ERROR_TIMEOUT => Error::Timeout,
        LIBUSB_ERROR_OVERFLOW => Error::Overflow,
        LIBUSB_ERROR_PIPE => Error::Pipe,
        LIBUSB_ERROR_INTERRUPTED => Error::Interrupted,
        LIBUSB_ERROR_NO_MEM => Error::NoMem,
        LIBUSB_ERROR_NOT_SUPPORTED => Error::NotSupported,
        _ => Error::Other,
    }
}

macro_rules! try_unsafe {
    ($x:expr) => {
        // SAFETY: expression is calling C library of libusb.
        match unsafe { $x } {
            0 => (),
            err => return Err(from_libusb(err)),
        }
    };
}

pub fn get_request_from_transfer(transfer: *mut libusb_transfer) -> Arc<Mutex<UsbHostRequest>> {
    // Safety: cast the raw pointer of transfer's user_data to the
    // Arc<Mutex<UsbHostRequest>>.
    unsafe { Arc::from_raw((*transfer).user_data.cast::<Mutex<UsbHostRequest>>()) }
}

pub fn get_buffer_from_transfer(transfer: *mut libusb_transfer) -> &'static mut [u8] {
    // SAFETY: cast the raw pointer of transfer's buffer which is transformed
    // from a slice with actual_length to a mutable slice.
    unsafe {
        std::slice::from_raw_parts_mut((*transfer).buffer, (*transfer).actual_length as usize)
    }
}

pub fn get_length_from_transfer(transfer: *mut libusb_transfer) -> i32 {
    // SAFETY: cast the raw pointer of transfer's actual_length to a integer.
    unsafe { (*transfer).actual_length }
}

pub fn get_status_from_transfer(transfer: *mut libusb_transfer) -> i32 {
    // SAFETY: cast the raw pointer of transfer's status which is to a integer.
    unsafe { (*transfer).status }
}

pub fn map_packet_status(status: i32) -> UsbPacketStatus {
    match status {
        LIBUSB_TRANSFER_COMPLETED => UsbPacketStatus::Success,
        LIBUSB_TRANSFER_ERROR => UsbPacketStatus::IoError,
        LIBUSB_TRANSFER_TIMED_OUT => UsbPacketStatus::IoError,
        LIBUSB_TRANSFER_CANCELLED => UsbPacketStatus::IoError,
        LIBUSB_TRANSFER_STALL => UsbPacketStatus::Stall,
        LIBUSB_TRANSFER_NO_DEVICE => UsbPacketStatus::NoDev,
        _ => UsbPacketStatus::Babble,
    }
}

pub fn get_libusb_pollfds(usbhost: Arc<Mutex<UsbHost>>) -> *const *mut libusb_pollfd {
    // SAFETY: call C library of libusb to get pointer of poll fd.
    unsafe { libusb_get_pollfds(usbhost.lock().unwrap().context.as_raw()) }
}

pub fn set_pollfd_notifiers(
    poll: *const *mut libusb_pollfd,
    notifiers: &mut Vec<EventNotifier>,
    handler: Rc<NotifierCallback>,
) {
    let mut i = 0;
    // SAFETY: have checked whether the pointer is null before dereference it.
    unsafe {
        loop {
            if (*poll.offset(i)).is_null() {
                break;
            };
            if (*(*poll.offset(i))).events as c_int == EPOLLIN {
                notifiers.push(EventNotifier::new(
                    NotifierOperation::AddShared,
                    (*(*poll.offset(i))).fd,
                    None,
                    EventSet::IN,
                    vec![handler.clone()],
                ));
            } else if (*(*poll.offset(i))).events as c_int == EPOLLOUT {
                notifiers.push(EventNotifier::new(
                    NotifierOperation::AddShared,
                    (*(*poll.offset(i))).fd,
                    None,
                    EventSet::OUT,
                    vec![handler.clone()],
                ));
            }
            i += 1;
        }
    }
}

pub fn alloc_host_transfer(iso_packets: c_int) -> *mut libusb_transfer {
    if iso_packets < 0 {
        error!(
            "The number of iso packets cannot be less than 0, it is {}",
            iso_packets
        );
        return std::ptr::null_mut();
    }

    // SAFETY: have checked the validity of iso_packets before call C
    // library of libusb to get the pointer of transfer.
    unsafe { libusb1_sys::libusb_alloc_transfer(iso_packets) }
}

extern "system" fn req_complete_data(host_transfer: *mut libusb_transfer) {
    // SAFETY: transfer is still valid because libusb just completed it
    // but we haven't told anyone yet. user_data remains valid because
    // it is dropped only when the request is completed and removed from
    // requests linked list.
    let request = get_request_from_transfer(host_transfer);
    let mut locked_request = request.lock().unwrap();
    let packet = locked_request.packet.clone();
    let mut locked_packet = packet.lock().unwrap();

    if !locked_packet.is_async {
        locked_request.complete();
        return;
    }

    let actual_length = get_length_from_transfer(host_transfer);
    let transfer_status = get_status_from_transfer(host_transfer);
    locked_packet.status = map_packet_status(transfer_status);

    if locked_packet.pid as u8 == USB_TOKEN_IN && actual_length != 0 {
        let data = get_buffer_from_transfer(host_transfer);
        locked_packet.transfer_packet(data, actual_length as usize);
    }

    if let Some(transfer) = locked_packet.xfer_ops.as_ref() {
        if let Some(ops) = transfer.clone().upgrade() {
            drop(locked_packet);
            ops.lock().unwrap().submit_transfer();
        }
    }

    locked_request.complete();
}

pub fn fill_bulk_transfer(
    transfer: *mut libusb_transfer,
    handle: Option<&mut DeviceHandle<Context>>,
    ep_number: u8,
    request: Arc<Mutex<UsbHostRequest>>,
) {
    let packet = request.lock().unwrap().packet.clone();
    let size = packet.lock().unwrap().get_iovecs_size();
    let buffer_ptr = request.lock().unwrap().buffer.as_mut_ptr();

    if handle.is_none() {
        error!("Failed to fill bulk transfer, handle is none");
        return;
    }

    if transfer.is_null() {
        error!("Failed to fill bulk transfer, transfer is none");
        return;
    }

    // SAFETY: have checked the validity of parameters of libusb_fill_bulk_transfer
    // before call libusb_fill_bulk_transfer.
    unsafe {
        libusb1_sys::libusb_fill_bulk_transfer(
            transfer,
            handle.unwrap().as_raw(),
            ep_number,
            buffer_ptr,
            size as i32,
            req_complete_data,
            (Arc::into_raw(request) as *mut Mutex<UsbHostRequest>).cast::<libc::c_void>(),
            BULK_TIMEOUT,
        );
    }
}

pub fn fill_interrupt_transfer(
    transfer: *mut libusb_transfer,
    handle: Option<&mut DeviceHandle<Context>>,
    ep_number: u8,
    request: Arc<Mutex<UsbHostRequest>>,
) {
    let packet = request.lock().unwrap().packet.clone();
    let size = packet.lock().unwrap().get_iovecs_size();
    let buffer_ptr = request.lock().unwrap().buffer.as_mut_ptr();

    if handle.is_none() {
        error!("Failed to fill interrupt transfer, handle is none");
        return;
    }

    if transfer.is_null() {
        error!("Failed to fill interrupt transfer, transfer is a null pointer");
        return;
    }

    // SAFETY: have checked the validity of parameters of libusb_fill_interrupt_transfer
    // before call libusb_fill_interrupt_transfer.
    unsafe {
        libusb1_sys::libusb_fill_interrupt_transfer(
            transfer,
            handle.unwrap().as_raw(),
            ep_number,
            buffer_ptr,
            size as i32,
            req_complete_data,
            (Arc::into_raw(request) as *mut Mutex<UsbHostRequest>).cast::<libc::c_void>(),
            INTERRUPT_TIMEOUT,
        );
    }
}

pub fn submit_host_transfer(transfer: *mut libusb_transfer) -> Result<()> {
    if transfer.is_null() {
        return Err(Error::NoMem);
    }
    try_unsafe!(libusb1_sys::libusb_submit_transfer(transfer));
    Ok(())
}

pub fn cancel_host_transfer(transfer: *mut libusb_transfer) -> Result<()> {
    if transfer.is_null() {
        return Ok(());
    }
    try_unsafe!(libusb1_sys::libusb_cancel_transfer(transfer));
    Ok(())
}

pub fn free_host_transfer(transfer: *mut libusb_transfer) {
    if transfer.is_null() {
        return;
    }

    // SAFETY: have checked the validity of transfer before call libusb_free_transfer.
    unsafe { libusb1_sys::libusb_free_transfer(transfer) };
}