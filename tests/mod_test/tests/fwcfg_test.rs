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

use byteorder::{ByteOrder, LittleEndian};
use devices::legacy::FwCfgEntryType;
use mod_test::libdriver::fwcfg::{bios_args, FW_CFG_BASE};
use mod_test::libdriver::machine::TestStdMachine;
use mod_test::libtest::test_init;
use mod_test::utils::{cleanup_img, create_img, TEST_IMAGE_SIZE};
use mod_test::utils::{swap_u16, swap_u32};

use std::cell::RefCell;
use std::process::Command;
use std::rc::Rc;
use std::{fs, mem};

// FwCfg Signature
const FW_CFG_DMA_SIGNATURE: u128 = 0x51454d5520434647;

#[test]
fn test_signature() {
    let mut args: Vec<&str> = Vec::new();
    bios_args(&mut args);
    let mut test_state = test_init(args);

    let mut read_data: Vec<u8> = Vec::with_capacity(4);
    let target_data: [u8; 4] = ['Q' as u8, 'E' as u8, 'M' as u8, 'U' as u8];

    // Select Signature entry and read it.
    test_state.fw_cfg_read_bytes(FwCfgEntryType::Signature as u16, &mut read_data, 4);
    assert_eq!(read_data.as_slice(), target_data);

    test_state.stop();
}

#[test]
fn test_id() {
    let mut args: Vec<&str> = Vec::new();
    bios_args(&mut args);
    let mut test_state = test_init(args);

    // Select Id entry and read it.
    let read_data = test_state.fw_cfg_read_u32(FwCfgEntryType::Id as u16);
    assert_eq!(read_data, 3);

    test_state.stop();
}

#[test]
fn test_nographic() {
    let mut args: Vec<&str> = Vec::new();
    bios_args(&mut args);
    let mut test_state = test_init(args);

    // Select NoGraphic entry and read it.
    let read_data = test_state.fw_cfg_read_u32(FwCfgEntryType::NoGraphic as u16);
    assert_eq!(read_data, 0);

    test_state.stop();
}

#[test]
fn test_nbcpus() {
    let mut args: Vec<&str> = Vec::new();
    bios_args(&mut args);
    let mut extra_args: Vec<&str> = "-smp 10".split(' ').collect();
    args.append(&mut extra_args);
    let mut test_state = test_init(args);

    // Select NbCpus entry and read it.
    let read_data = test_state.fw_cfg_read_u16(FwCfgEntryType::NbCpus as u16);
    assert_eq!(read_data, 10);

    test_state.stop();
}

#[test]
fn test_kernel_initrd_cmdlint() {
    let mut args: Vec<&str> = Vec::new();
    bios_args(&mut args);

    assert!(cfg!(target_os = "linux"));
    let kernel_path = "/tmp/kernel";
    let initrd_path = "/tmp/initrd";
    let kernel_of = format!("of={}", kernel_path);
    let initrd_of = format!("of={}", initrd_path);
    let mut output = Command::new("dd")
        .arg("if=/dev/zero")
        .arg(&kernel_of)
        .arg("bs=1M")
        .arg("count=10")
        .output()
        .expect("Failed to create tmp kernel");
    assert!(output.status.success());

    output = Command::new("dd")
        .arg("if=/dev/zero")
        .arg(&initrd_of)
        .arg("bs=1M")
        .arg("count=1")
        .output()
        .expect("Failed to create tmp initrd");
    assert!(output.status.success());

    let kernel_para = format!("-kernel {}", kernel_path);
    let initrd_para = format!("-initrd {}", initrd_path);
    let mut extra_args: Vec<&str> = kernel_para.split(' ').collect();
    args.append(&mut extra_args);
    extra_args = initrd_para.split(' ').collect();
    args.append(&mut extra_args);
    extra_args = "-m 1G".split(' ').collect();
    args.append(&mut extra_args);

    // set cmdlint
    let cmdline = "-append console=ttyS0 root=/dev/vda reboot=k panic=1";
    extra_args = cmdline.split(' ').collect();
    args.append(&mut extra_args);
    let mut test_state = test_init(args);

    // Select KernelSize entry and read it.
    let read_data = test_state.fw_cfg_read_u32(FwCfgEntryType::KernelSize as u16);
    assert_eq!(read_data, 10 * 1024 * 1024);

    // Select InitrdAddr entry and read it.
    let read_data = test_state.fw_cfg_read_u32(FwCfgEntryType::InitrdAddr as u16);
    // Initrd addr = (mem start) + (mem end) - (initrd size)
    let initrd_addr = 0x4000_0000 + 0x4000_0000 - 0x10_0000;
    assert_eq!(read_data, initrd_addr);

    // Select CmdlineSize entry and read it.
    let read_data = test_state.fw_cfg_read_u32(FwCfgEntryType::CmdlineSize as u16);
    // cmdline size = cmdline - "-append".
    let cmdline_size = cmdline.to_string().len() as u32 - 8;
    assert_eq!(read_data, cmdline_size + 1);

    // Select CmdlineData entry and read it.
    let mut read_data: Vec<u8> = Vec::with_capacity(cmdline_size as usize);
    test_state.fw_cfg_read_bytes(
        FwCfgEntryType::CmdlineData as u16,
        &mut read_data,
        cmdline_size,
    );
    assert_eq!(String::from_utf8_lossy(&read_data), cmdline[8..]);

    fs::remove_file(kernel_path).expect("Failed to remove the kernel file");
    fs::remove_file(initrd_path).expect("Failed to remove the initrd file");
    test_state.stop();
}

#[test]
fn test_filedir_by_dma() {
    let mut args: Vec<&str> = Vec::new();
    bios_args(&mut args);
    let test_state = Rc::new(RefCell::new(test_init(args)));
    let machine = TestStdMachine::new(test_state.clone());
    let allocator = machine.allocator.clone();

    let file_name = "etc/boot-fail-wait";
    let mut read_data: Vec<u8> = Vec::with_capacity(mem::size_of::<u32>());

    // Select FileDir entry and read it.
    let file_size = test_state.borrow().fw_cfg_read_file(
        &mut allocator.borrow_mut(),
        file_name,
        &mut read_data,
        mem::size_of::<u32>() as u32,
    );
    assert_eq!(file_size, mem::size_of::<u32>() as u32);

    let time_out = LittleEndian::read_u32(&read_data);
    assert_eq!(time_out, 5);

    test_state.borrow_mut().stop();
}

#[test]
fn test_boot_index() {
    let mut args: Vec<&str> = Vec::new();
    bios_args(&mut args);

    let image_path = create_img(TEST_IMAGE_SIZE, 0);

    let dev_path = "/pci@ffffffffffffffff/scsi@1/disk@0,0\n\0".to_string();

    let mut extra_args =
        "-device virtio-blk-pci,id=drv0,drive=drive0,bus=pcie.0,addr=0x1.0,bootindex=0"
            .split(' ')
            .collect();
    args.append(&mut extra_args);

    let image_para = format!(
        "-drive if=none,id=drive0,file={},format=raw,direct=false",
        image_path
    );
    extra_args = image_para.split(' ').collect();
    args.append(&mut extra_args);

    let test_state = Rc::new(RefCell::new(test_init(args)));
    let machine = TestStdMachine::new(test_state.clone());
    let allocator = machine.allocator.clone();

    let file_name = "bootorder";
    let mut read_data: Vec<u8> = Vec::with_capacity(dev_path.len());

    // Select FileDir entry and read it.
    let file_size = test_state.borrow().fw_cfg_read_file(
        &mut allocator.borrow_mut(),
        file_name,
        &mut read_data,
        dev_path.len() as u32,
    );
    assert_eq!(file_size, dev_path.len() as u32);
    assert_eq!(&read_data, dev_path.as_bytes());

    test_state.borrow_mut().stop();
    if !image_path.is_empty() {
        cleanup_img(image_path)
    }
}

#[test]
fn test_smbios_tyep0() {
    let mut args: Vec<&str> = Vec::new();
    bios_args(&mut args);

    let mut extra_args = "-smbios type=0,vendor=vendor0,version=version0,date=date0"
        .split(' ')
        .collect();
    args.append(&mut extra_args);

    let test_state = Rc::new(RefCell::new(test_init(args)));
    let machine = TestStdMachine::new(test_state.clone());
    let allocator = machine.allocator.clone();

    let anchor_file = "etc/smbios/smbios-anchor";
    let tables_file = "etc/smbios/smbios-tables";
    let mut read_data: Vec<u8> = Vec::with_capacity(24);

    // Select FileDir entry and read it.
    let anchor_size = test_state.borrow().fw_cfg_read_file(
        &mut allocator.borrow_mut(),
        anchor_file,
        &mut read_data,
        24 as u32,
    );

    assert_eq!(anchor_size, 24 as u32);
    assert_eq!(String::from_utf8_lossy(&read_data[..5]), "_SM3_");
    assert_eq!(read_data[6], 24 as u8);
    let talble_len = LittleEndian::read_u32(&read_data[12..]);
    assert_eq!(talble_len, 109);

    let mut read_table_date: Vec<u8> = Vec::with_capacity(talble_len as usize);
    let talbles_size = test_state.borrow().fw_cfg_read_file(
        &mut allocator.borrow_mut(),
        tables_file,
        &mut read_table_date,
        talble_len,
    );
    assert_eq!(talbles_size, talble_len);
    let talbe_type0_len = 24;
    assert_eq!(
        String::from_utf8_lossy(&read_table_date[talbe_type0_len..talbe_type0_len + 7]),
        "vendor0"
    );
    assert_eq!(
        String::from_utf8_lossy(&read_table_date[talbe_type0_len + 8..talbe_type0_len + 16]),
        "version0"
    );
    assert_eq!(
        String::from_utf8_lossy(&read_table_date[talbe_type0_len + 17..talbe_type0_len + 22]),
        "date0"
    );

    test_state.borrow_mut().stop();
}

#[test]
fn test_smbios_tyep1() {
    let mut args: Vec<&str> = Vec::new();
    bios_args(&mut args);

    let mut extra_args = "-smbios type=0,vendor=vendor0,version=version0,date=date0"
        .split(' ')
        .collect();
    args.append(&mut extra_args);

    let mut extra_args = "-smbios type=1,manufacturer=manufacturer1,product=product1,\
    version=12.2.2,serial=181a6bdf-ff98-4c5e-97ec-bff35fe41f6c,uuid=181a6bdf-ff98-4c5e-97ec-bff35fe41f6c,\
    family=Virtual,sku=sku1"
        .split(' ')
        .collect();
    args.append(&mut extra_args);

    let test_state = Rc::new(RefCell::new(test_init(args)));
    let machine = TestStdMachine::new(test_state.clone());
    let allocator = machine.allocator.clone();

    let anchor_file = "etc/smbios/smbios-anchor";
    let tables_file = "etc/smbios/smbios-tables";
    let mut read_data: Vec<u8> = Vec::with_capacity(24);

    // Select FileDir entry and read it.
    let anchor_size = test_state.borrow().fw_cfg_read_file(
        &mut allocator.borrow_mut(),
        anchor_file,
        &mut read_data,
        24 as u32,
    );

    assert_eq!(anchor_size, 24 as u32);
    assert_eq!(String::from_utf8_lossy(&read_data[..5]), "_SM3_");
    assert_eq!(read_data[6], 24 as u8);
    let talble_len = LittleEndian::read_u32(&read_data[12..]);
    assert_eq!(talble_len, 162);

    let mut read_table_date: Vec<u8> = Vec::with_capacity(talble_len as usize);
    let talbles_size = test_state.borrow().fw_cfg_read_file(
        &mut allocator.borrow_mut(),
        tables_file,
        &mut read_table_date,
        talble_len,
    );
    assert_eq!(talbles_size, talble_len);
    let talbe_type0_len = 24;
    assert_eq!(
        String::from_utf8_lossy(&read_table_date[talbe_type0_len..talbe_type0_len + 7]),
        "vendor0"
    );
    assert_eq!(
        String::from_utf8_lossy(&read_table_date[talbe_type0_len + 8..talbe_type0_len + 16]),
        "version0"
    );
    assert_eq!(read_table_date[48], 1);
    assert_eq!(read_table_date[49], 27 as u8);
    let handle1 = LittleEndian::read_u16(&read_table_date[50..]);
    assert_eq!(handle1, 0x100);

    assert_eq!(
        String::from_utf8_lossy(&read_table_date[75..88]),
        "manufacturer1"
    );
    assert_eq!(
        String::from_utf8_lossy(&read_table_date[89..97]),
        "product1"
    );
    assert_eq!(String::from_utf8_lossy(&read_table_date[98..104]), "12.2.2");
    assert_eq!(
        String::from_utf8_lossy(&read_table_date[105..141]),
        "181a6bdf-ff98-4c5e-97ec-bff35fe41f6c"
    );
    assert_eq!(String::from_utf8_lossy(&read_table_date[142..146]), "sku1");
    assert_eq!(
        String::from_utf8_lossy(&read_table_date[147..154]),
        "Virtual"
    );
    // check uuid
    assert_eq!(read_table_date[56], 0xdf);
    assert_eq!(read_table_date[57], 0x6b);
    assert_eq!(read_table_date[58], 0x1a);
    assert_eq!(read_table_date[59], 0x18);

    assert_eq!(read_table_date[60], 0x98);
    assert_eq!(read_table_date[61], 0xff);

    assert_eq!(read_table_date[62], 0x5e);
    assert_eq!(read_table_date[63], 0x4c);

    assert_eq!(read_table_date[64], 0x97);
    assert_eq!(read_table_date[65], 0xec);

    assert_eq!(read_table_date[66], 0xbf);
    assert_eq!(read_table_date[67], 0xf3);
    assert_eq!(read_table_date[68], 0x5f);
    assert_eq!(read_table_date[69], 0xe4);
    assert_eq!(read_table_date[70], 0x1f);
    assert_eq!(read_table_date[71], 0x6c);

    test_state.borrow_mut().stop();
}

#[test]
fn test_exception_by_ctrl_reg() {
    let mut args = Vec::new();
    bios_args(&mut args);
    let mut test_state = test_init(args);

    // Select Signature entry and read it by control register.
    test_state.writew(FW_CFG_BASE, swap_u16(FwCfgEntryType::Signature as u16));
    let read_data = test_state.readw(FW_CFG_BASE + 0x8);

    // Read data by control register always return 0.
    assert_eq!(read_data, 0);

    test_state.stop();
}

#[test]
fn test_exception_scenarios() {
    let mut args = Vec::new();
    bios_args(&mut args);
    let mut test_state = test_init(args);

    // Select entry which is not exit and read it.
    let read_data = test_state.fw_cfg_read_u32(0xffff);
    assert_eq!(read_data, 0);

    // Read data exceeds the original size.
    let read_data = test_state.fw_cfg_read_u32(FwCfgEntryType::Id as u16);
    assert_eq!(read_data, 3);
    assert_eq!(test_state.readl(FW_CFG_BASE), 0);

    // Read data offset: 0x17 + size: 4 > 0x18, which is overflow
    assert_eq!(test_state.readl(FW_CFG_BASE + 0x17), 0);

    // Read FW_CFG_DMA_SIGNATURE high 32bit
    assert_eq!(
        swap_u32(test_state.readl(FW_CFG_BASE + 0x10)),
        (FW_CFG_DMA_SIGNATURE >> 32) as u32
    );

    test_state.stop();
}
