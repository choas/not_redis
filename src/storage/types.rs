//! Internal data types for the storage engine.

use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;

/// Internal data types stored in the engine.
///
/// These represent the actual data structures that can be stored,
/// as opposed to the RESP protocol [`Value`](crate::Value) types.
#[derive(Debug, Clone)]
pub enum RedisData {
    String(SmallVec<[u8; 64]>),
    List(VecDeque<SmallVec<[u8; 64]>>),
    Set(FxHashSet<SmallVec<[u8; 64]>>),
    Hash(FxHashMap<SmallVec<[u8; 64]>, SmallVec<[u8; 64]>>),
    ZSet(BTreeMap<SmallVec<[u8; 64]>, f64>),
}

/// A value stored in the storage engine with optional expiration.
#[derive(Debug, Clone)]
pub struct StoredValue {
    pub data: RedisData,
    pub expire_at: Option<Instant>,
}

impl StoredValue {
    pub fn is_expired(&self) -> bool {
        match self.expire_at {
            Some(at) => Instant::now() >= at,
            None => false,
        }
    }

    pub fn estimated_size(&self) -> usize {
        self.data.estimated_size()
    }
}

impl RedisData {
    pub fn estimated_size(&self) -> usize {
        match self {
            RedisData::String(data) => data.len(),
            RedisData::List(deque) => {
                deque.iter().map(|v| v.len()).sum::<usize>()
                    + std::mem::size_of::<SmallVec<[u8; 64]>>() * deque.capacity()
            }
            RedisData::Set(set) => {
                set.iter().map(|v| v.len()).sum::<usize>()
                    + std::mem::size_of::<SmallVec<[u8; 64]>>() * set.capacity()
            }
            RedisData::Hash(map) => {
                map.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>()
                    + std::mem::size_of::<(SmallVec<[u8; 64]>, SmallVec<[u8; 64]>)>() * map.capacity()
            }
            RedisData::ZSet(map) => {
                map.iter().map(|(k, _)| k.len()).sum::<usize>()
                    + std::mem::size_of::<(SmallVec<[u8; 64]>, f64)>() * map.capacity()
                    + std::mem::size_of::<f64>()
            }
        }
    }
}
