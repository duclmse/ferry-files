//! Lazily built orders over the entries, one per sort key.
//!
//! An order is only built when a query first asks for that key, so an index
//! nobody sorts costs nothing. After that, changed entries are queued and
//! merged in on the next sorted query: a full re-sort of millions of names
//! for every change on disk would make a busy tree as slow as having no
//! order at all.

use std::cmp::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use rayon::prelude::*;

use super::search::scan_pool;
use super::state::{EntryRef, IndexState};
use super::{RecordView, SortKey};
use crate::store::cmp_name_nocase;

/// Below this many queued entries an order is always kept and merged.
const MIN_DIRTY_BEFORE_DROP: usize = 4096;
/// Past `order.len() / DIRTY_DIVISOR` queued entries, rebuilding beats merging.
const DIRTY_DIVISOR: usize = 8;

#[derive(Default)]
struct Slot {
    order: Option<Arc<Vec<u32>>>,
    dirty: Vec<u32>,
}

#[derive(Default)]
pub(super) struct SortIndexes {
    slots: [Mutex<Slot>; SortKey::COUNT],
}

fn compare(key: SortKey, a: &EntryRef<'_>, b: &EntryRef<'_>) -> Ordering {
    let by_key = match key {
        SortKey::Name => cmp_name_nocase(a.name_str(), b.name_str()),
        SortKey::Size => a.size_bytes().cmp(&b.size_bytes()),
        SortKey::DateModified => a.mtime().cmp(&b.mtime()),
        SortKey::DateCreated => a.ctime().cmp(&b.ctime()),
    };
    by_key.then(a.index().cmp(&b.index()))
}

pub(super) fn sort_numbers(state: &IndexState, key: SortKey, numbers: &mut [u32]) {
    let cmp = |a: &u32, b: &u32| compare(key, &state.view(*a), &state.view(*b));
    match scan_pool() {
        Some(pool) => pool.install(|| numbers.par_sort_unstable_by(cmp)),
        None => numbers.sort_unstable_by(cmp),
    }
}

fn build(state: &IndexState, key: SortKey) -> Vec<u32> {
    let mut order: Vec<u32> = state.iter().map(|e| e.index()).collect();
    sort_numbers(state, key, &mut order);
    order
}

/// `order` without the `dirty` entries, merged with the ones still live.
fn merge_dirty(state: &IndexState, key: SortKey, order: &[u32], dirty: &mut Vec<u32>) -> Vec<u32> {
    let mut queued = vec![false; state.slots()];
    for &i in dirty.iter() {
        queued[i as usize] = true;
    }
    dirty.sort_unstable();
    dirty.dedup();
    dirty.retain(|&i| state.is_live(i));
    sort_numbers(state, key, dirty);

    let mut merged = Vec::with_capacity(order.len() + dirty.len());
    let mut fresh = dirty.iter().copied().peekable();
    for &i in order.iter().filter(|&&i| !queued[i as usize]) {
        while let Some(f) =
            fresh.next_if(|&f| compare(key, &state.view(f), &state.view(i)) == Ordering::Less)
        {
            merged.push(f);
        }
        merged.push(i);
    }
    merged.extend(fresh);
    merged
}

impl SortIndexes {
    fn each_slot(&mut self) -> impl Iterator<Item = &mut Slot> {
        self.slots
            .iter_mut()
            .map(|m| m.get_mut().unwrap_or_else(PoisonError::into_inner))
    }

    /// Entry `i` was added, changed or removed.
    pub(super) fn touch(&mut self, i: u32) {
        for slot in self.each_slot() {
            let Some(order) = &slot.order else { continue };
            if slot.dirty.len() >= (order.len() / DIRTY_DIVISOR).max(MIN_DIRTY_BEFORE_DROP) {
                *slot = Slot::default();
            } else {
                slot.dirty.push(i);
            }
        }
    }

    /// Entry numbers changed, so every stored order is meaningless.
    pub(super) fn clear(&mut self) {
        for slot in self.each_slot() {
            *slot = Slot::default();
        }
    }

    /// Live entries ascending by `key`, ties in entry order.
    pub(super) fn order(&self, state: &IndexState, key: SortKey) -> Arc<Vec<u32>> {
        let mut slot = self.slots[key as usize]
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let slot = &mut *slot;
        let order = match (&slot.order, slot.dirty.is_empty()) {
            (Some(order), true) => return Arc::clone(order),
            (Some(order), false) => merge_dirty(state, key, order, &mut slot.dirty),
            (None, _) => build(state, key),
        };
        slot.dirty.clear();
        let order = Arc::new(order);
        slot.order = Some(Arc::clone(&order));
        order
    }
}
