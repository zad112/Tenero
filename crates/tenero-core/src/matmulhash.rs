//! matmulhash v2 on the CPU, portable and unoptimised: `docs/CONSENSUS.md` section 8.2, checked bit
//! for bit against `tests/vectors/matmulhash_*.json` and `pow_misc.json`.
//!
//! The dataset is kept as raw bytes: a slice's bytes are the transposed matrix, so
//! `W[t][n] = raw[n * k + t]`. Words are little-endian `u32`.

use crate::chacha20;
use crate::hash::{hex_lower, sha256};
use crate::u256::U256;

/// A dataset block is 16 words = 64 bytes = one ChaCha20 block.
pub const BLOCK_WORDS: usize = 16;
pub const BLOCK_BYTES: usize = 64;
/// Earlier blocks each new block picks (data-dependently) and mixes in.
const PICKS: usize = 3;
const DATASET_LABEL: &[u8] = b"tenero matmulhash v2 dataset";
const EPOCH_0_LABEL: &[u8] = b"tenero matmulhash epoch 0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    /// Rows of X per attempt.
    pub m: usize,
    /// The shared dimension.
    pub k: usize,
    /// Columns of one slice (a slice is `k * nb` bytes).
    pub nb: usize,
    /// Slices in the dataset.
    pub num_blocks: usize,
}

impl Params {
    /// The chain default: a 4 GiB dataset of 256 slices of 16 MiB.
    pub const DEFAULT: Params = Params {
        m: 64,
        k: 8192,
        nb: 2048,
        num_blocks: 256,
    };

    /// The rules of `CONSENSUS.md` section 8.2, checked without overflow.
    pub fn validate(&self) -> Result<(), String> {
        let Params {
            m,
            k,
            nb,
            num_blocks,
        } = *self;
        if m.min(k).min(nb).min(num_blocks) < 1 {
            return Err("m, k, nb and num_blocks must be positive".into());
        }
        if k % 8 != 0 || nb % 8 != 0 {
            return Err("k and nb must be multiples of 8".into());
        }
        let mk = m.checked_mul(k).ok_or("m*k overflows")?;
        let mnb = m.checked_mul(nb).ok_or("m*nb overflows")?;
        if mk % 64 != 0 || mnb % 16 != 0 {
            return Err("m*k must be a multiple of 64 and m*nb a multiple of 16".into());
        }
        let kk = k.checked_mul(128 * 128).ok_or("k is too large")?;
        if kk >= 1 << 31 {
            return Err("k is too large: int32 accumulation could overflow".into());
        }
        let slice_bytes = k.checked_mul(nb).ok_or("k*nb overflows")?;
        num_blocks
            .checked_mul(slice_bytes)
            .ok_or("the dataset is too large")?;
        if slice_bytes / BLOCK_BYTES >= 1 << 32 || num_blocks >= 1 << 32 {
            return Err("dataset too large for 32-bit block counters".into());
        }
        Ok(())
    }

    pub fn slice_bytes(&self) -> usize {
        self.k * self.nb
    }

    pub fn blocks_per_slice(&self) -> usize {
        self.slice_bytes() / BLOCK_BYTES
    }
}

// ------------------------------------------------------------------ epochs and targets

/// The seed of epoch 0 is `sha256("tenero matmulhash epoch 0")`; each next one is the hash of the last.
pub fn epoch_seed(epoch: u64) -> [u8; 32] {
    let mut seed = sha256(&[EPOCH_0_LABEL]);
    for _ in 0..epoch {
        seed = sha256(&[&seed]);
    }
    seed
}

/// Blocks `1..=epoch_blocks` are epoch 0. `None` for the genesis block (index 0) or `epoch_blocks == 0`.
pub fn epoch_of(index: u64, epoch_blocks: u64) -> Option<u64> {
    if index == 0 || epoch_blocks == 0 {
        return None;
    }
    Some((index - 1) / epoch_blocks)
}

pub fn dataset_key(epoch_seed: &[u8; 32]) -> [u8; 32] {
    sha256(&[DATASET_LABEL, epoch_seed])
}

/// A target that needs about 2^bits attempts on average: `2^(256 - bits)`, for `1 <= bits <= 256`.
pub fn bits_to_target(bits: u32) -> Option<U256> {
    if !(1..=256).contains(&bits) {
        return None;
    }
    U256::pow2(256 - bits)
}

// ------------------------------------------------------------------ the dataset

fn read_block(bytes: &[u8]) -> [u32; BLOCK_WORDS] {
    let mut w = [0u32; BLOCK_WORDS];
    for (i, word) in w.iter_mut().enumerate() {
        *word = u32::from_le_bytes([
            bytes[4 * i],
            bytes[4 * i + 1],
            bytes[4 * i + 2],
            bytes[4 * i + 3],
        ]);
    }
    w
}

fn write_block(bytes: &mut [u8], w: &[u32; BLOCK_WORDS]) {
    for (i, word) in w.iter().enumerate() {
        bytes[4 * i..4 * i + 4].copy_from_slice(&word.to_le_bytes());
    }
}

/// Builds blocks `first_block..` of slice `j >= 1` into `out`, reading slice `j - 1` and the earlier
/// slices from `done` (slices `0..j`). Blocks of one slice depend only on earlier slices, so any
/// ranges of one slice can be built in any order or at the same time.
fn fill_blocks(done: &[u8], j: usize, first_block: usize, out: &mut [u8], p: &Params) {
    let slice_bytes = p.slice_bytes();
    let blocks = p.blocks_per_slice();
    let prev_slice = &done[(j - 1) * slice_bytes..j * slice_bytes];
    for i in 0..out.len() / BLOCK_BYTES {
        let u = first_block + i;
        let prev = read_block(&prev_slice[u * BLOCK_BYTES..(u + 1) * BLOCK_BYTES]);
        let mut r = [0u32; BLOCK_WORDS];
        for pick in 0..PICKS {
            let s = prev[2 * pick] as usize % j;
            let b = prev[2 * pick + 1] as usize % blocks;
            let at = s * slice_bytes + b * BLOCK_BYTES;
            let picked = read_block(&done[at..at + BLOCK_BYTES]);
            for (acc, x) in r.iter_mut().zip(&picked) {
                *acc ^= x;
            }
        }
        let mut state = [0u32; BLOCK_WORDS];
        state[0..4].copy_from_slice(&chacha20::CONSTANTS);
        for w in 0..8 {
            state[4 + w] = prev[w] ^ r[w];
        }
        state[12] = u as u32; // the block number (below 2^32: see `validate`)
        state[13] = j as u32; // the slice number
        state[14] = prev[8] ^ r[8];
        state[15] = prev[9] ^ r[9];
        let mut o = chacha20::core(&state);
        for ((x, a), b) in o.iter_mut().zip(&prev).zip(&r) {
            *x ^= a ^ b; // every input word is used
        }
        write_block(&mut out[i * BLOCK_BYTES..(i + 1) * BLOCK_BYTES], &o);
    }
}

/// The first `slices` slices of one epoch's dataset (all of them when `slices == num_blocks`).
pub struct Dataset {
    params: Params,
    slices: usize,
    bytes: Vec<u8>,
}

impl Dataset {
    /// Builds the first `slices` slices (a slice only depends on the ones before it, so a prefix is
    /// the same bytes as in the whole dataset). `threads` changes the speed, never the result.
    pub fn build(
        params: &Params,
        epoch_seed: &[u8; 32],
        slices: usize,
        threads: usize,
    ) -> Result<Dataset, String> {
        params.validate()?;
        if slices == 0 || slices > params.num_blocks {
            return Err(format!("slices must be 1..={}", params.num_blocks));
        }
        let slice_bytes = params.slice_bytes();
        let blocks = params.blocks_per_slice();
        let mut bytes = vec![0u8; slices * slice_bytes];

        let key = chacha20::key_words(&dataset_key(epoch_seed));
        for (u, chunk) in (0..blocks).zip(bytes[..slice_bytes].chunks_mut(BLOCK_BYTES)) {
            // counter = the block number
            write_block(chunk, &chacha20::block_words(&key, u as u32, [0; 3]));
        }

        let threads = threads.max(1);
        let per_thread = blocks.div_ceil(threads) * BLOCK_BYTES;
        for j in 1..slices {
            let (done, rest) = bytes.split_at_mut(j * slice_bytes);
            let done: &[u8] = done;
            let cur = &mut rest[..slice_bytes];
            if threads == 1 {
                fill_blocks(done, j, 0, cur, params);
            } else {
                std::thread::scope(|scope| {
                    for (n, part) in cur.chunks_mut(per_thread).enumerate() {
                        let first = n * per_thread / BLOCK_BYTES;
                        scope.spawn(move || fill_blocks(done, j, first, part, params));
                    }
                });
            }
        }
        Ok(Dataset {
            params: *params,
            slices,
            bytes,
        })
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// How many slices were built.
    pub fn slices(&self) -> usize {
        self.slices
    }

    pub fn slice(&self, b: usize) -> Option<&[u8]> {
        let sb = self.params.slice_bytes();
        (b < self.slices).then(|| &self.bytes[b * sb..(b + 1) * sb])
    }

    /// All the bytes built so far, slice after slice.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

// ------------------------------------------------------------------ one attempt

pub fn attempt_seed(header_hash: &[u8; 32], nonce: u64) -> [u8; 32] {
    sha256(&[header_hash, &nonce.to_le_bytes()])
}

/// Which dataset slice this attempt reads (independent of the bytes that make X).
pub fn attempt_slice(seed: &[u8; 32], num_blocks: usize) -> usize {
    let h = sha256(&[seed, &[1u8]]);
    let mut eight = [0u8; 8];
    eight.copy_from_slice(&h[..8]);
    (u64::from_le_bytes(eight) % num_blocks as u64) as usize
}

/// The `m x k` int8 matrix of this attempt (row-major): the ChaCha20 keystream keyed by the seed.
pub fn make_x(seed: &[u8; 32], p: &Params) -> Vec<i8> {
    let stream = chacha20::keystream(seed, p.m * p.k / 64, 0, [0; 3]);
    stream.into_iter().map(|b| b as i8).collect()
}

/// `C = X @ W` with exact integer arithmetic: an `m x nb` row-major int32 matrix. With
/// `W[t][n] = raw[n*k + t]`, entry `(i, n)` is the dot product of row `i` of X with row `n` of `raw`.
pub fn product(x: &[i8], slice: &[u8], p: &Params) -> Vec<i32> {
    product_of_columns(x, |n| &slice[n * p.k..(n + 1) * p.k], p)
}

/// `C = X @ W` where column `n` of W is the `k` bytes `column(n)` (as int8): the product of both designs.
fn product_of_columns<'a>(x: &[i8], column: impl Fn(usize) -> &'a [u8], p: &Params) -> Vec<i32> {
    let mut c = vec![0i32; p.m * p.nb];
    for n in 0..p.nb {
        let w = column(n);
        for i in 0..p.m {
            let row = &x[i * p.k..(i + 1) * p.k];
            c[i * p.nb + n] = row
                .iter()
                .zip(w)
                .map(|(&a, &b)| i32::from(a) * i32::from(b as i8))
                .sum();
        }
    }
    c
}

/// The fold: each 16-word chunk of C (the int32 values reinterpreted as uint32), with its number
/// XORed into word 0, goes through the ChaCha20 core; the output words are added into 8 sums.
/// `c.len()` must be a multiple of 16.
pub fn fold_sums(c: &[i32]) -> [u64; 8] {
    assert!(
        c.len().is_multiple_of(16),
        "the fold needs whole chunks of 16 words"
    );
    let mut sums = [0u64; 8];
    for (number, chunk) in c.chunks(16).enumerate() {
        let mut state = [0u32; 16];
        for (w, v) in state.iter_mut().zip(chunk) {
            *w = *v as u32;
        }
        state[0] ^= number as u32;
        let out = chacha20::core(&state);
        for i in 0..8 {
            sums[i] = sums[i].wrapping_add(u64::from(out[i]) + u64::from(out[i + 8]));
        }
    }
    sums
}

/// The 8 sums as 64 little-endian bytes.
pub fn mix_bytes(sums: &[u64; 8]) -> [u8; 64] {
    let mut mix = [0u8; 64];
    for (i, s) in sums.iter().enumerate() {
        mix[8 * i..8 * i + 8].copy_from_slice(&s.to_le_bytes());
    }
    mix
}

pub fn digest_of(seed: &[u8; 32], mix: &[u8]) -> [u8; 32] {
    sha256(&[seed, mix])
}

/// Every intermediate of one attempt (the vectors check them all).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub seed: [u8; 32],
    pub slice_index: usize,
    pub sums: [u64; 8],
    pub mix: [u8; 64],
    pub digest: [u8; 32],
}

/// The full recomputation of one attempt. Needs the slice this attempt reads to have been built.
pub fn compute_attempt(
    data: &Dataset,
    header_hash: &[u8; 32],
    nonce: u64,
) -> Result<Attempt, String> {
    let p = data.params();
    let seed = attempt_seed(header_hash, nonce);
    let slice_index = attempt_slice(&seed, p.num_blocks);
    let slice = data.slice(slice_index).ok_or_else(|| {
        format!(
            "slice {slice_index} was not built ({} slices built)",
            data.slices()
        )
    })?;
    let c = product(&make_x(&seed, p), slice, p);
    let sums = fold_sums(&c);
    let mix = mix_bytes(&sums);
    let digest = digest_of(&seed, &mix);
    Ok(Attempt {
        seed,
        slice_index,
        sums,
        mix,
        digest,
    })
}

// ------------------------------------------------------------------ the gathered attempt (from the fork height)
//
// From a network's gather fork height on (beta and dev: 500; `docs/CONSENSUS.md` section 8.3), an attempt multiplies X by
// `nb` columns picked one by one from the WHOLE dataset instead of one slice. The slice of the first design is two
// cheap hashes of the nonce, so a miner can try nonces 16 to a slice and read each slice once for all 16, which makes
// the proof of work limited by multiply speed, not memory (`THREAT_MODEL.md` E11); columns picked one by one from
// 2^19 leave two attempts sharing about 8 of 2048, each column with different partners. Column `j` of the dataset is
// bytes `[j*k, (j+1)*k)` of the whole dataset, so column `n` of slice `b` is column `b*nb + n`.

/// The key of a gathered attempt's column picks: `SHA-256(seed || 0x02)`.
pub fn pick_key(seed: &[u8; 32]) -> [u8; 32] {
    sha256(&[seed, &[2u8]])
}

/// The `nb` dataset columns a gathered attempt reads: little-endian u32 word `n` of the ChaCha20 keystream of
/// `pick_key(seed)` (counter 0, nonce 0, `ceil(nb / 16)` blocks), modulo `num_blocks * nb`.
pub fn pick_columns(seed: &[u8; 32], p: &Params) -> Vec<u32> {
    let columns = (p.num_blocks * p.nb) as u64;
    let stream = chacha20::keystream(&pick_key(seed), p.nb.div_ceil(16), 0, [0; 3]);
    stream
        .as_chunks::<4>()
        .0
        .iter()
        .take(p.nb)
        .map(|w| (u64::from(u32::from_le_bytes(*w)) % columns) as u32)
        .collect()
}

/// `C = X @ W` where column `n` of W is dataset column `cols[n]` (`pick_columns`). Needs the whole dataset.
pub fn gathered_product(x: &[i8], data: &Dataset, cols: &[u32]) -> Result<Vec<i32>, String> {
    let p = data.params();
    let columns = p.num_blocks * p.nb;
    if data.slices() != p.num_blocks
        || cols.len() != p.nb
        || cols.iter().any(|&c| c as usize >= columns)
    {
        return Err("a gathered product needs the whole dataset and nb columns inside it".into());
    }
    let bytes = data.bytes();
    Ok(product_of_columns(
        x,
        |n| {
            let j = cols[n] as usize;
            &bytes[j * p.k..(j + 1) * p.k]
        },
        p,
    ))
}

/// The full recomputation of one GATHERED attempt (`Attempt::slice_index` is 0: there is no slice). Needs the WHOLE
/// dataset, since the columns are anywhere in it.
pub fn compute_gathered_attempt(
    data: &Dataset,
    header_hash: &[u8; 32],
    nonce: u64,
) -> Result<Attempt, String> {
    let p = data.params();
    if data.slices() != p.num_blocks {
        return Err(format!(
            "a gathered attempt needs the whole dataset ({} of {} slices built)",
            data.slices(),
            p.num_blocks
        ));
    }
    let seed = attempt_seed(header_hash, nonce);
    let c = gathered_product(&make_x(&seed, p), data, &pick_columns(&seed, p))?;
    let sums = fold_sums(&c);
    let mix = mix_bytes(&sums);
    let digest = digest_of(&seed, &mix);
    Ok(Attempt {
        seed,
        slice_index: 0,
        sums,
        mix,
        digest,
    })
}

/// The attempt a chain requires at `height`: gathered from `gather_from` on (`u64::MAX`: never), the first design
/// before it.
pub fn compute_attempt_at(
    data: &Dataset,
    header_hash: &[u8; 32],
    nonce: u64,
    height: u64,
    gather_from: u64,
) -> Result<Attempt, String> {
    if height >= gather_from {
        compute_gathered_attempt(data, header_hash, nonce)
    } else {
        compute_attempt(data, header_hash, nonce)
    }
}

/// A hash is valid when, as a big-endian integer, it is strictly below the target.
pub fn meets_target(digest: &[u8; 32], target: &U256) -> bool {
    U256::from_be_bytes(digest) < *target
}

/// The CHEAP check (no dataset): the claimed mix and nonce produce this hash, and it meets the
/// target. A block that fails cannot be valid; one that passes still needs the full check, because
/// a forger can grind a made-up mix. The nonce is a `u64`, which is the range rule `0 <= nonce < 2^64`.
/// The hash is compared as lower-case hex.
///
/// `target: None` checks only that the block is self-consistent (the mix and nonce give this hash),
/// with no limit on the hash. The Python reference does this by passing a target of exactly 2^256,
/// which needs 257 bits and so is not a `U256`; `None` is that case.
pub fn precheck(
    header_hash: &[u8; 32],
    nonce: u64,
    mix: &[u8],
    block_hash_hex: &str,
    target: Option<&U256>,
) -> bool {
    if mix.len() != 64 {
        return false;
    }
    let digest = digest_of(&attempt_seed(header_hash, nonce), mix);
    hex_lower(&digest) == block_hash_hex && target.is_none_or(|t| meets_target(&digest, t))
}

/// The full check of a block's proof of work: the recomputed digest meets the target.
pub fn verify(
    data: &Dataset,
    header_hash: &[u8; 32],
    nonce: u64,
    target: &U256,
) -> Result<bool, String> {
    Ok(meets_target(
        &compute_attempt(data, header_hash, nonce)?.digest,
        target,
    ))
}
