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
    cell::RefCell,
    collections::{hash_map::Iter, HashMap},
    rc::Rc,
};

use anyhow::{bail, Result};
use byteorder::{BigEndian, ByteOrder};
use log::warn;

const CACHE_DEFAULT_SIZE: usize = 1;
pub const ENTRY_SIZE_U16: usize = 2;
pub const ENTRY_SIZE_U64: usize = 8;

#[derive(Clone)]
pub struct DirtyInfo {
    /// If the entry is marked dirty, it needs to be rewritten back to the disk.
    pub is_dirty: bool,
    /// The start of the dirty area.
    pub start: u64,
    /// The end of the dirty area.
    pub end: u64,
}

impl Default for DirtyInfo {
    fn default() -> Self {
        Self {
            is_dirty: false,
            start: u64::MAX,
            end: 0,
        }
    }
}

impl DirtyInfo {
    pub fn clear(&mut self) {
        self.is_dirty = false;
        self.start = u64::MAX;
        self.end = 0;
    }
}

#[derive(Clone, Default)]
pub struct CacheTable {
    /// If the table is marked dirty, it needs to be rewritten back to the disk.
    pub dirty_info: DirtyInfo,
    /// Lru hit count.
    pub lru_count: u64,
    /// Host offset of cached table.
    pub addr: u64,
    /// The size of an entry in bytes.
    entry_size: usize,
    /// Buffer of table data.
    table_data: Vec<u8>,
}

impl CacheTable {
    pub fn new(addr: u64, table_data: Vec<u8>, entry_size: usize) -> Result<Self> {
        if entry_size == 0 {
            bail!("Invalid entry size");
        }
        Ok(Self {
            dirty_info: Default::default(),
            lru_count: 0,
            addr,
            entry_size,
            table_data,
        })
    }

    fn be_read(&self, idx: usize) -> Result<u64> {
        let start = idx * self.entry_size;
        let end = start + self.entry_size;
        if end > self.table_data.len() {
            bail!("Invalid idx {}", idx);
        }
        let v = match self.entry_size {
            ENTRY_SIZE_U16 => BigEndian::read_u16(&self.table_data[start..end]) as u64,
            ENTRY_SIZE_U64 => BigEndian::read_u64(&self.table_data[start..end]),
            _ => bail!("Unsupported entry size {}", self.entry_size),
        };
        Ok(v)
    }

    #[inline(always)]
    pub fn get_entry_map(&mut self, idx: usize) -> Result<u64> {
        self.be_read(idx)
    }

    #[inline(always)]
    pub fn set_entry_map(&mut self, idx: usize, value: u64) -> Result<()> {
        let start = idx * self.entry_size;
        let end = start + self.entry_size;
        if end > self.table_data.len() {
            bail!("Invalid idx {}", idx);
        }
        match self.entry_size {
            ENTRY_SIZE_U16 => BigEndian::write_u16(&mut self.table_data[start..end], value as u16),
            ENTRY_SIZE_U64 => BigEndian::write_u64(&mut self.table_data[start..end], value),
            _ => bail!("Unsupported entry size {}", self.entry_size),
        }

        let mut dirty_info = &mut self.dirty_info;
        dirty_info.start = std::cmp::min(dirty_info.start, start as u64);
        dirty_info.end = std::cmp::max(dirty_info.end, end as u64);
        dirty_info.is_dirty = true;
        Ok(())
    }

    pub fn find_empty_entry(&self, start: usize) -> Result<usize> {
        let len = self.table_data.len() / self.entry_size;
        for i in start..len {
            if self.be_read(i)? == 0 {
                return Ok(i);
            }
        }
        Ok(len)
    }

    pub fn get_value(&self) -> &[u8] {
        &self.table_data
    }
}

#[derive(Clone, Default)]
pub struct Qcow2Cache {
    /// Max size of the cache map.
    pub max_size: usize,
    /// LRU count which record the latest count and increased when cache is accessed.
    pub lru_count: u64,
    pub cache_map: HashMap<u64, Rc<RefCell<CacheTable>>>,
}

impl Qcow2Cache {
    pub fn new(mut max_size: usize) -> Self {
        if max_size == 0 {
            max_size = CACHE_DEFAULT_SIZE;
            warn!(
                "The cache max size is 0, use the default value {}",
                CACHE_DEFAULT_SIZE
            );
        }
        Self {
            max_size,
            lru_count: 0,
            cache_map: HashMap::with_capacity(max_size),
        }
    }

    fn check_refcount(&mut self) {
        if self.lru_count < u64::MAX {
            return;
        }
        warn!("refcount reaches the max limit and is reset to 0");
        for (_, entry) in self.cache_map.iter() {
            entry.borrow_mut().lru_count = 0;
        }
    }

    pub fn contains_keys(&self, key: u64) -> bool {
        self.cache_map.contains_key(&key)
    }

    pub fn get(&mut self, key: u64) -> Option<&Rc<RefCell<CacheTable>>> {
        self.check_refcount();
        let entry = self.cache_map.get(&key)?;
        // LRU replace algorithm.
        entry.borrow_mut().lru_count = self.lru_count;
        self.lru_count += 1;
        Some(entry)
    }

    pub fn iter(&self) -> Iter<u64, Rc<RefCell<CacheTable>>> {
        self.cache_map.iter()
    }

    pub fn lru_replace(
        &mut self,
        key: u64,
        entry: Rc<RefCell<CacheTable>>,
    ) -> Option<Rc<RefCell<CacheTable>>> {
        let mut replaced_entry: Option<Rc<RefCell<CacheTable>>> = None;
        let mut lru_count = u64::MAX;
        let mut target_idx = 0;
        self.check_refcount();
        entry.borrow_mut().lru_count = self.lru_count;
        self.lru_count += 1;

        if self.cache_map.len() < self.max_size {
            self.cache_map.insert(key, entry);
            return replaced_entry;
        }

        for (key, entry) in self.cache_map.iter() {
            let borrowed_entry = entry.borrow();
            if borrowed_entry.lru_count < lru_count {
                lru_count = borrowed_entry.lru_count;
                replaced_entry = Some(entry.clone());
                target_idx = *key;
            }
        }
        self.cache_map.remove(&target_idx);
        self.cache_map.insert(key, entry);
        replaced_entry
    }
}

#[cfg(test)]
mod test {
    use super::{CacheTable, Qcow2Cache};
    use std::{cell::RefCell, rc::Rc};

    #[test]
    fn test_cache_entry() {
        let buf: Vec<u64> = vec![0x00, 0x01, 0x02, 0x03, 0x04];
        let mut vec = Vec::new();
        for i in 0..buf.len() {
            vec.append(&mut buf[i].to_be_bytes().to_vec());
        }
        let mut entry = CacheTable::new(0x00 as u64, vec, 8).unwrap();
        assert_eq!(entry.get_entry_map(0).unwrap(), 0x00);
        assert_eq!(entry.get_entry_map(3).unwrap(), 0x03);
        assert_eq!(entry.get_entry_map(4).unwrap(), 0x04);

        entry.set_entry_map(0x02, 0x09).unwrap();
        assert_eq!(entry.get_entry_map(2).unwrap(), 0x09);
    }

    #[test]
    fn test_qcow2_cache() {
        let buf: Vec<u64> = vec![0x00, 0x01, 0x02, 0x03, 0x04];
        let mut vec = Vec::new();
        for i in 0..buf.len() {
            vec.append(&mut buf[i].to_be_bytes().to_vec());
        }
        let entry_0 = Rc::new(RefCell::new(
            CacheTable::new(0x00 as u64, vec.clone(), 8).unwrap(),
        ));
        entry_0.borrow_mut().lru_count = 0;
        let entry_1 = Rc::new(RefCell::new(
            CacheTable::new(0x00 as u64, vec.clone(), 8).unwrap(),
        ));
        entry_1.borrow_mut().lru_count = 1;
        let entry_2 = Rc::new(RefCell::new(
            CacheTable::new(0x00 as u64, vec.clone(), 8).unwrap(),
        ));
        entry_2.borrow_mut().lru_count = 2;
        let entry_3 = Rc::new(RefCell::new(
            CacheTable::new(0x00 as u64, vec.clone(), 8).unwrap(),
        ));
        entry_3.borrow_mut().lru_count = 3;

        let mut qcow2_cache: Qcow2Cache = Qcow2Cache::new(3);
        assert!(qcow2_cache.lru_replace(0x00, entry_0).is_none());
        assert!(qcow2_cache.lru_replace(0x01, entry_1).is_none());
        assert!(qcow2_cache.lru_replace(0x02, entry_2).is_none());
        assert!(qcow2_cache.lru_replace(0x03, entry_3).is_some());
    }
}
