//! Choosing nonces so that the attempts of a batch share slices.
//!
//! An attempt's slice is `attempt_slice(attempt_seed(header, nonce))`: two SHA-256 hashes, known before any matrix
//! work. A miner may try its nonces in any order, so it can collect nonces by slice and run `group` attempts against
//! each slice together: the GPU then reads a 16 MiB slice once for `group` attempts (`Int8Gemm::run_rows`) instead of
//! once for each. **Nothing about the proof of work changes**: every attempt is the same attempt, with the same
//! chance of meeting the target; only the order of trying them does.
//!
//! The scan goes up from the first nonce (wrapping at 2^64) and every nonce it looks at is kept until its slice has
//! `group` of them, so none is skipped while the grouper lives. When a job ends, the nonces still waiting were never
//! tried (they are not counted as attempts either).

use tenero_core::matmulhash as mh;

/// Attempts to make, grouped by slice: `nonces[i]` has seed `seeds[i]` and reads slice `slices[i]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Batch {
    pub nonces: Vec<u64>,
    pub seeds: Vec<[u8; 32]>,
    pub slices: Vec<usize>,
}

impl Batch {
    pub fn len(&self) -> usize {
        self.nonces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nonces.is_empty()
    }
}

/// Collects the nonces of one header by slice, from a first nonce upwards.
pub struct SliceGrouper {
    header_hash: [u8; 32],
    num_blocks: usize,
    group: usize,
    next: u64,
    scanned: u64,
    waiting: Vec<Vec<(u64, [u8; 32])>>,
}

impl SliceGrouper {
    /// `group` attempts per slice (at least 1; 1 is the plain order, nothing waits).
    pub fn new(
        header_hash: [u8; 32],
        num_blocks: usize,
        group: usize,
        first_nonce: u64,
    ) -> SliceGrouper {
        assert!(num_blocks >= 1 && group >= 1);
        SliceGrouper {
            header_hash,
            num_blocks,
            group,
            next: first_nonce,
            scanned: 0,
            waiting: vec![Vec::new(); num_blocks],
        }
    }

    /// Nonces looked at so far (tried or waiting).
    pub fn scanned(&self) -> u64 {
        self.scanned
    }

    /// Nonces looked at and not yet handed out.
    pub fn waiting(&self) -> usize {
        self.waiting.iter().map(Vec::len).sum()
    }

    /// The next `groups` whole groups: `groups * group` attempts, `group` of them for each slice in the batch (a
    /// slice may appear in more than one group). Their order is the order the groups filled up in.
    pub fn next_batch(&mut self, groups: usize) -> Batch {
        let mut out = Batch::default();
        let want = groups * self.group;
        while out.len() < want {
            let nonce = self.next;
            self.next = self.next.wrapping_add(1);
            self.scanned += 1;
            let seed = mh::attempt_seed(&self.header_hash, nonce);
            let slice = mh::attempt_slice(&seed, self.num_blocks);
            let w = &mut self.waiting[slice];
            w.push((nonce, seed));
            if w.len() == self.group {
                for (n, s) in w.drain(..) {
                    out.nonces.push(n);
                    out.seeds.push(s);
                    out.slices.push(slice);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn every_group_is_whole_and_reads_one_slice_and_every_seed_and_slice_is_the_real_one() {
        let hh = [7u8; 32];
        let mut g = SliceGrouper::new(hh, 16, 4, 1000);
        let b = g.next_batch(10);
        assert_eq!(b.len(), 40);
        for chunk in 0..10 {
            let s = b.slices[chunk * 4];
            assert!(b.slices[chunk * 4..chunk * 4 + 4].iter().all(|&x| x == s));
        }
        for i in 0..b.len() {
            assert_eq!(b.seeds[i], mh::attempt_seed(&hh, b.nonces[i]));
            assert_eq!(b.slices[i], mh::attempt_slice(&b.seeds[i], 16));
        }
    }

    #[test]
    fn no_nonce_is_handed_out_twice_or_skipped() {
        let hh = [3u8; 32];
        let mut g = SliceGrouper::new(hh, 8, 5, 0);
        let mut seen = HashSet::new();
        for _ in 0..50 {
            for n in g.next_batch(3).nonces {
                assert!(seen.insert(n), "nonce {n} twice");
            }
        }
        // everything scanned is either handed out or still waiting, and the scan is 0..scanned with no gap
        assert_eq!(seen.len() + g.waiting(), g.scanned() as usize);
        let waiting: HashSet<u64> = g.waiting.iter().flatten().map(|(n, _)| *n).collect();
        for n in 0..g.scanned() {
            assert!(seen.contains(&n) ^ waiting.contains(&n), "nonce {n}");
        }
    }

    #[test]
    fn a_group_of_one_is_the_plain_order() {
        let mut g = SliceGrouper::new([1u8; 32], 256, 1, 42);
        assert_eq!(g.next_batch(5).nonces, vec![42, 43, 44, 45, 46]);
        assert_eq!(g.next_batch(2).nonces, vec![47, 48]);
        assert_eq!(g.waiting(), 0);
    }

    #[test]
    fn the_scan_wraps_at_two_to_the_64() {
        let mut g = SliceGrouper::new([2u8; 32], 2, 3, u64::MAX - 1);
        let b = g.next_batch(4);
        assert!(b.nonces.contains(&u64::MAX) && b.nonces.contains(&0));
    }

    #[test]
    fn in_the_steady_state_little_is_waiting() {
        // 256 slices, groups of 16: at most 15 wait per slice, whatever has been handed out
        let mut g = SliceGrouper::new([5u8; 32], 256, 16, 0);
        for _ in 0..20 {
            g.next_batch(32);
            assert!(g.waiting() <= 256 * 15);
        }
        let mut per: HashMap<usize, usize> = HashMap::new();
        for s in g.next_batch(64).slices {
            *per.entry(s).or_default() += 1;
        }
        assert!(per.values().all(|&c| c % 16 == 0));
    }
}
