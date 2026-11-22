//! Block skip filters for name searches.
//!
//! Each block of `FILTER_BLOCK` entry numbers has a Bloom filter of its live
//! names' 3-byte windows, ASCII-lowercased. A search whose name conditions
//! require some literal text tests that text's windows first and reads only
//! the blocks that may hold all of them. A term that matches nothing used to
//! read every name, ~25-40 ms at 9M real names; with the filters it reads a
//! few blocks (`benches/store_scale.rs trigram`).
//!
//! Filters are built on first use, under the read lock, and dropped when an
//! entry in the block gets a different name. Removing an entry leaves its
//! windows set, which can only make a block look like a candidate.

use std::sync::OnceLock;

use super::matching::Compiled;

/// Entries per filter. 32768 (the scan block) skipped no-match terms but few
/// blocks for rare real terms, since a block that large holds tens of
/// thousands of distinct windows; 1024 cost twice the memory and checking
/// the filters ate the gain. At 4096: 1.6 B per name.
pub(super) const FILTER_BLOCK: usize = 4096;
/// ~0.8% of absent windows test as present.
const BITS_PER_KEY: usize = 10;
const PROBES: u64 = 7;

pub(super) struct Bloom {
    bits: Box<[u64]>,
}

fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// The filter's bits for `key`. Both hashes must be full width: `fastrange`
/// takes the high bits, and a 32-bit step put every probe on the same bit.
fn probes(key: u32, nbits: u64) -> impl Iterator<Item = usize> {
    let h1 = mix(u64::from(key));
    let h2 = mix(h1 ^ 0x9e37_79b9_7f4a_7c15) | 1;
    (0..PROBES).map(move |i| {
        let x = h1.wrapping_add(i.wrapping_mul(h2));
        ((u128::from(x) * u128::from(nbits)) >> 64) as usize
    })
}

impl Bloom {
    fn new(keys: &[u32]) -> Self {
        let words = (keys.len() * BITS_PER_KEY).div_ceil(64).max(1);
        let mut bits = vec![0u64; words].into_boxed_slice();
        let nbits = words as u64 * 64;
        for &key in keys {
            for bit in probes(key, nbits) {
                bits[bit / 64] |= 1 << (bit % 64);
            }
        }
        Self { bits }
    }

    fn may_hold(&self, key: u32) -> bool {
        let nbits = self.bits.len() as u64 * 64;
        probes(key, nbits).all(|bit| self.bits[bit / 64] & (1 << (bit % 64)) != 0)
    }
}

/// `text`'s 3-byte windows, folded as the name matchers fold (ASCII only).
pub(super) fn windows(text: &[u8], out: &mut Vec<u32>) {
    for w in text.windows(3) {
        let [a, b, c] = [w[0], w[1], w[2]].map(|x| u32::from(x.to_ascii_lowercase()));
        out.push(a | b << 8 | c << 16);
    }
}

pub(super) fn build<'a>(names: impl Iterator<Item = &'a str>) -> Bloom {
    let mut keys = Vec::new();
    for name in names {
        windows(name.as_bytes(), &mut keys);
    }
    keys.sort_unstable();
    keys.dedup();
    Bloom::new(&keys)
}

#[derive(Default)]
pub(super) struct NameFilters {
    blocks: Vec<OnceLock<Bloom>>,
}

impl NameFilters {
    /// Unbuilt filters for `slots` entry numbers.
    pub(super) fn new(slots: usize) -> Self {
        Self {
            blocks: (0..slots.div_ceil(FILTER_BLOCK))
                .map(|_| OnceLock::new())
                .collect(),
        }
    }

    /// Entry `i` was added or renamed.
    pub(super) fn changed(&mut self, i: u32) {
        let b = i as usize / FILTER_BLOCK;
        if b >= self.blocks.len() {
            self.blocks.resize_with(b + 1, OnceLock::new);
        } else {
            self.blocks[b] = OnceLock::new();
        }
    }

    /// Block `b`'s filter, built from `names` if it has none. `None` for a
    /// block with no filter slot, which is then always read.
    pub(super) fn get(&self, b: usize, names: impl FnOnce() -> Bloom) -> Option<&Bloom> {
        Some(self.blocks.get(b)?.get_or_init(names))
    }

    pub(super) fn heap_bytes(&self) -> usize {
        self.blocks.capacity() * std::mem::size_of::<OnceLock<Bloom>>()
            + self
                .blocks
                .iter()
                .filter_map(OnceLock::get)
                .map(|f| f.bits.len() * 8)
                .sum::<usize>()
    }
}

/// Text a matching name must contain, as windows: all of `windows`, and for
/// each `either` group at least one of its alternatives.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Need {
    windows: Vec<u32>,
    either: Vec<Vec<Need>>,
}

impl Need {
    /// What `conds`, all of which must match, require of a name; `None` if
    /// they require nothing a filter can test.
    pub(super) fn of(conds: &[Compiled<'_>]) -> Option<Self> {
        let mut need = Self::default();
        for c in conds {
            match c {
                Compiled::Or(branches) => {
                    let alternatives: Option<Vec<Need>> =
                        branches.iter().map(|b| Self::of(b)).collect();
                    // A branch that needs nothing lets every block through.
                    if let Some(alternatives) = alternatives {
                        need.either.push(alternatives);
                    }
                }
                other => {
                    for literal in other.name_literals() {
                        windows(literal, &mut need.windows);
                    }
                }
            }
        }
        need.windows.sort_unstable();
        need.windows.dedup();
        (!need.windows.is_empty() || !need.either.is_empty()).then_some(need)
    }

    pub(super) fn allows(&self, filter: &Bloom) -> bool {
        self.windows.iter().all(|&w| filter.may_hold(w))
            && self
                .either
                .iter()
                .all(|alternatives| alternatives.iter().any(|n| n.allows(filter)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> Vec<u32> {
        let mut k = Vec::new();
        windows(text.as_bytes(), &mut k);
        k
    }

    #[test]
    fn a_filter_holds_every_window_it_was_built_from() {
        let names = ["Report_2024.PDF", "main.rs", "日本語.txt"];
        let filter = build(names.iter().copied());
        for name in names {
            for k in keys(&name.to_ascii_lowercase()) {
                assert!(filter.may_hold(k), "{name}");
            }
        }
    }

    #[test]
    fn absent_windows_rarely_test_as_present() {
        let names: Vec<String> = (0..4096).map(|i| format!("file_{i}_name.txt")).collect();
        let filter = build(names.iter().map(String::as_str));
        let present: std::collections::HashSet<u32> = names.iter().flat_map(|n| keys(n)).collect();
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let (mut tried, mut hit) = (0, 0);
        while tried < 100_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let k = (x as u32) & 0x00ff_ffff;
            if !present.contains(&k) {
                tried += 1;
                hit += usize::from(filter.may_hold(k));
            }
        }
        assert!(hit < tried / 50, "{hit} of {tried} absent windows passed");
    }
}
