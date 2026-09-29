//! The store: appending, reading back, refusing bad blocks, rolling blocks back, pruning, reopening, and
//! a randomised comparison with a plain in-memory model.

use std::path::PathBuf;
use tenero_core::hash::{sha256, Sha256Stream};
use tenero_core::v2::ids::{self, PowKind};
use tenero_core::v2::*;
use tenero_store::{Store, StoreError, StoredOutput};

const LABEL: &str = "tenero store test network";
const POW: PowKind = PowKind::Sha256;

/// A database file in the temp directory, deleted when the test ends.
struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> TempDb {
        let p = std::env::temp_dir().join(format!(
            "tenero-store-test-{}-{name}.redb",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        TempDb(p)
    }

    fn open(&self) -> Store {
        Store::open(&self.0, LABEL, POW).unwrap()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ------------------------------------------------------------------ making blocks

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        let mut b = [0u8; N];
        for c in b.iter_mut() {
            *c = self.next() as u8;
        }
        b
    }
}

fn work_for(height: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&(height * 1000).to_be_bytes());
    w
}

/// Builds valid-looking blocks (the store does not check proofs), each on top of the previous one.
struct Maker {
    rng: Rng,
    next_key: u64,
    proof_len: usize,
    txs_per_block: usize,
}

impl Maker {
    fn new(seed: u64) -> Maker {
        Maker {
            rng: Rng(seed | 1),
            next_key: 0,
            proof_len: 200,
            txs_per_block: 2,
        }
    }

    fn key_image(&mut self) -> [u8; 32] {
        self.next_key += 1;
        sha256(&[b"key image", &self.next_key.to_le_bytes()])
    }

    fn tx(&mut self) -> Transaction {
        let inputs = (0..2)
            .map(|_| Input {
                key_image: self.key_image(),
                ring: (0..16).collect(), // a ring of the real size (the store does not look at it)
            })
            .collect();
        let outputs = (0..2)
            .map(|_| Output {
                onetime_address: self.rng.bytes(),
                amount_commitment: self.rng.bytes(),
                amount_enc: self.rng.bytes(),
                view_tag: self.rng.bytes(),
                ephemeral_pubkey: self.rng.bytes(),
                anchor_enc: self.rng.bytes(),
            })
            .collect();
        let mut proof = vec![0u8; self.proof_len];
        for c in proof.iter_mut() {
            *c = self.rng.next() as u8;
        }
        Transaction {
            prefix: TxPrefix {
                version: VERSION,
                inputs,
                outputs,
                fee: 1000,
                extra: vec![],
            },
            proof_data: proof,
        }
    }

    fn block(&mut self, prev_id: [u8; 32], height: u64, txs: Vec<Transaction>) -> Block {
        let coinbase = Coinbase {
            version: VERSION,
            height,
            outputs: vec![CoinbaseOutput {
                onetime_address: self.rng.bytes(),
                amount: 2_000_000_000,
                view_tag: self.rng.bytes(),
                ephemeral_pubkey: self.rng.bytes(),
                anchor_enc: self.rng.bytes(),
            }],
            extra: vec![],
        };
        let tx_root = ids::block_tx_root(&coinbase, &txs).unwrap();
        let header = BlockHeader {
            version: VERSION,
            prev_id,
            timestamp: 1_700_000_000 + 60 * height,
            tx_root,
            nonce: self.rng.next(),
            mix: [0; 64],
        };
        Block {
            header,
            coinbase,
            transactions: txs,
        }
    }

    fn next_block(&mut self, prev_id: [u8; 32], height: u64) -> Block {
        let txs = (0..self.txs_per_block).map(|_| self.tx()).collect();
        self.block(prev_id, height, txs)
    }
}

/// A store and the blocks appended to it, in order (`blocks[0]` is height 1).
struct Chain {
    store: Store,
    maker: Maker,
    blocks: Vec<Block>,
}

impl Chain {
    fn new(db: &TempDb, seed: u64) -> Chain {
        Chain {
            store: db.open(),
            maker: Maker::new(seed),
            blocks: vec![],
        }
    }

    fn tip_id(&self) -> [u8; 32] {
        match self.blocks.last() {
            Some(b) => ids::block_id(&b.header, POW),
            None => ids::genesis_id(LABEL),
        }
    }

    fn push(&mut self) {
        let height = self.blocks.len() as u64 + 1;
        let block = self.maker.next_block(self.tip_id(), height);
        self.store.append_block(&block, work_for(height)).unwrap();
        self.blocks.push(block);
    }

    fn push_n(&mut self, n: usize) {
        for _ in 0..n {
            self.push();
        }
    }

    fn pop(&mut self) -> StoredBlockCheck {
        let popped = self.store.pop_block().unwrap();
        let original = self.blocks.pop().expect("a block to pop");
        StoredBlockCheck { popped, original }
    }
}

struct StoredBlockCheck {
    popped: tenero_store::StoredBlock,
    original: Block,
}

/// The state digest computed from a list of blocks alone, the way `Store::state_digest` defines it:
/// every output in index order, then every spent key image in byte order.
fn model_digest(blocks: &[Block]) -> [u8; 32] {
    let mut outputs: Vec<StoredOutput> = Vec::new();
    let mut images: Vec<([u8; 32], u64)> = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        let height = i as u64 + 1;
        for o in &b.coinbase.outputs {
            outputs.push(StoredOutput {
                onetime_address: o.onetime_address,
                amount_commitment: [0; 32],
                public_amount: o.amount,
                height,
                coinbase: true,
            });
        }
        for t in &b.transactions {
            for inp in &t.prefix.inputs {
                images.push((inp.key_image, height));
            }
            for o in &t.prefix.outputs {
                outputs.push(StoredOutput {
                    onetime_address: o.onetime_address,
                    amount_commitment: o.amount_commitment,
                    public_amount: 0,
                    height,
                    coinbase: false,
                });
            }
        }
    }
    images.sort();
    let mut h = Sha256Stream::new();
    h.update(b"tenero state v2");
    h.update(&(outputs.len() as u64).to_le_bytes());
    for (i, o) in outputs.iter().enumerate() {
        h.update(&(i as u64).to_le_bytes());
        h.update(&o.to_bytes().unwrap());
    }
    h.update(&(images.len() as u64).to_le_bytes());
    for (ki, height) in &images {
        h.update(ki);
        h.update(&height.to_le_bytes());
    }
    h.finalize()
}

// ------------------------------------------------------------------ tests

#[test]
fn a_new_store_holds_only_the_genesis_block() {
    let db = TempDb::new("genesis");
    let s = db.open();
    let (height, tip) = s.tip().unwrap();
    assert_eq!(height, 0);
    assert_eq!(tip.block_id, ids::genesis_id(LABEL));
    assert_eq!(s.chain_id(), ids::genesis_id(LABEL));
    assert_eq!(tip.header, ids::genesis_header(LABEL));
    assert_eq!(
        (s.output_count().unwrap(), s.pruned_below().unwrap()),
        (0, 0)
    );
    assert_eq!(s.height_of(&ids::genesis_id(LABEL)).unwrap(), Some(0));
    assert!(s.get_block(0).unwrap().is_none() && s.get_block(1).unwrap().is_none());
    assert_eq!(s.state_digest().unwrap(), model_digest(&[]));
}

#[test]
fn blocks_are_stored_indexed_and_read_back_exactly() {
    let db = TempDb::new("readback");
    let mut c = Chain::new(&db, 1);
    c.push_n(20);
    let s = &c.store;
    assert_eq!(s.tip().unwrap().0, 20);
    let mut expected_first = 0u64;
    for (i, b) in c.blocks.iter().enumerate() {
        let height = i as u64 + 1;
        let stored = s.get_block(height).unwrap().unwrap();
        assert!(!stored.is_pruned());
        assert_eq!(
            stored.index.first_output_index, expected_first,
            "height {height}"
        );
        let n_out = b.coinbase.outputs.len()
            + b.transactions
                .iter()
                .map(|t| t.prefix.outputs.len())
                .sum::<usize>();
        assert_eq!(stored.index.output_count as usize, n_out);
        assert_eq!(stored.index.block_id, ids::block_id(&b.header, POW));
        assert_eq!(stored.index.cumulative_work, work_for(height));
        assert_eq!(s.height_of(&stored.index.block_id).unwrap(), Some(height));
        assert_eq!(&stored.into_full().unwrap(), b, "height {height}");
        // the documented order: the coinbase's outputs, then each transaction's, in order
        let mut idx = expected_first;
        for o in &b.coinbase.outputs {
            let got = s.output(idx).unwrap().unwrap();
            assert!(
                got.coinbase && got.public_amount == o.amount && got.amount_commitment == [0; 32]
            );
            assert_eq!(
                (got.onetime_address, got.height),
                (o.onetime_address, height)
            );
            idx += 1;
        }
        for t in &b.transactions {
            for o in &t.prefix.outputs {
                let got = s.output(idx).unwrap().unwrap();
                assert!(!got.coinbase && got.public_amount == 0);
                assert_eq!(
                    (got.onetime_address, got.amount_commitment),
                    (o.onetime_address, o.amount_commitment)
                );
                idx += 1;
            }
            for inp in &t.prefix.inputs {
                assert_eq!(s.key_image_height(&inp.key_image).unwrap(), Some(height));
            }
            let (h, st) = s.tx(&ids::tx_id(t).unwrap()).unwrap().unwrap();
            assert_eq!((h, st.proof_data.as_ref()), (height, Some(&t.proof_data)));
            assert_eq!(st.tx, t.prune().unwrap());
        }
        expected_first = idx;
    }
    assert_eq!(s.output_count().unwrap(), expected_first);
    assert!(s.output(expected_first).unwrap().is_none());
    assert_eq!(s.state_digest().unwrap(), model_digest(&c.blocks));
}

#[test]
fn a_refused_block_leaves_no_trace() {
    let db = TempDb::new("refused");
    let mut c = Chain::new(&db, 2);
    c.push_n(5);
    let before = (
        c.store.state_digest().unwrap(),
        c.store.tip().unwrap(),
        c.store.output_count().unwrap(),
    );
    let height = 6;
    let tip = c.tip_id();

    let mut m = Maker::new(99);
    m.next_key = 1_000_000;
    let good = m.next_block(tip, height);

    // every way a block can be inconsistent with the store
    let mut wrong_parent = good.clone();
    wrong_parent.header.prev_id = [7; 32];
    assert_eq!(
        c.store.append_block(&wrong_parent, work_for(6)),
        Err(StoreError::BadParent)
    );

    let one_tx = m.tx();
    let wrong_height = m.block(tip, 9, vec![one_tx]);
    assert_eq!(
        c.store.append_block(&wrong_height, work_for(6)),
        Err(StoreError::BadHeight {
            expected: 6,
            got: 9
        })
    );

    let mut wrong_version = good.clone();
    wrong_version.header.version = 3;
    assert_eq!(
        c.store.append_block(&wrong_version, work_for(6)),
        Err(StoreError::BadVersion(3))
    );

    let mut wrong_root = good.clone();
    wrong_root.header.tx_root = [1; 32];
    assert_eq!(
        c.store.append_block(&wrong_root, work_for(6)),
        Err(StoreError::BadTxRoot)
    );

    // a key image spent earlier in the chain
    let mut respend = good.clone();
    respend.transactions[1].prefix.inputs[0].key_image =
        c.blocks[2].transactions[0].prefix.inputs[1].key_image;
    respend.header.tx_root = ids::block_tx_root(&respend.coinbase, &respend.transactions).unwrap();
    assert_eq!(
        c.store.append_block(&respend, work_for(6)),
        Err(StoreError::DoubleSpend(
            c.blocks[2].transactions[0].prefix.inputs[1].key_image
        ))
    );

    // the same key image twice inside one block, in the SECOND transaction, so the first has been written
    // by the time it is noticed: everything must be rolled back
    let mut twice = good.clone();
    let dup = twice.transactions[0].prefix.inputs[0].key_image;
    twice.transactions[1].prefix.inputs[1].key_image = dup;
    twice.header.tx_root = ids::block_tx_root(&twice.coinbase, &twice.transactions).unwrap();
    assert_eq!(
        c.store.append_block(&twice, work_for(6)),
        Err(StoreError::DoubleSpend(dup))
    );

    // and none of it left anything behind
    let after = (
        c.store.state_digest().unwrap(),
        c.store.tip().unwrap(),
        c.store.output_count().unwrap(),
    );
    assert_eq!(before, after);
    assert!(c.store.block_index(6).unwrap().is_none());
    assert!(c
        .store
        .tx(&ids::tx_id(&twice.transactions[0]).unwrap())
        .unwrap()
        .is_none());
    assert!(c.store.key_image_height(&dup).unwrap().is_none());
    assert!(c
        .store
        .height_of(&ids::block_id(&twice.header, POW))
        .unwrap()
        .is_none());
    // the good block still goes in afterwards
    c.store.append_block(&good, work_for(6)).unwrap();
    assert_eq!(c.store.tip().unwrap().0, 6);
}

#[test]
fn rolling_blocks_back_restores_the_exact_previous_state() {
    let db = TempDb::new("pop");
    let mut c = Chain::new(&db, 3);
    let mut digests = vec![c.store.state_digest().unwrap()];
    let mut counts = vec![0u64];
    for _ in 0..30 {
        c.push();
        digests.push(c.store.state_digest().unwrap());
        counts.push(c.store.output_count().unwrap());
    }
    let all = c.blocks.clone();
    for height in (1..=30usize).rev() {
        let check = c.pop();
        assert_eq!(
            check.popped.clone().into_full().as_ref(),
            Some(&check.original),
            "the popped block is returned"
        );
        assert_eq!(c.store.tip().unwrap().0, height as u64 - 1);
        assert_eq!(
            c.store.state_digest().unwrap(),
            digests[height - 1],
            "state after removing block {height}"
        );
        assert_eq!(c.store.output_count().unwrap(), counts[height - 1]);
        for t in &check.original.transactions {
            assert!(c.store.tx(&ids::tx_id(t).unwrap()).unwrap().is_none());
            for i in &t.prefix.inputs {
                assert!(c.store.key_image_height(&i.key_image).unwrap().is_none());
            }
        }
        assert!(c
            .store
            .height_of(&check.popped.index.block_id)
            .unwrap()
            .is_none());
    }
    assert_eq!(c.store.pop_block(), Err(StoreError::CannotPopGenesis));
    // putting the same blocks back gives the same states, so global indexes are reused
    for (i, b) in all.iter().enumerate() {
        c.store.append_block(b, work_for(i as u64 + 1)).unwrap();
        assert_eq!(c.store.state_digest().unwrap(), digests[i + 1]);
    }
}

#[test]
fn pruning_removes_only_proofs_and_changes_no_id_root_or_state() {
    let db = TempDb::new("prune");
    let mut c = Chain::new(&db, 4);
    c.maker.proof_len = 1900;
    c.push_n(60);
    let s = &c.store;
    let digest = s.state_digest().unwrap();

    let stats = s.prune_below(40).unwrap();
    // blocks 1..=39, two transactions each
    assert_eq!(stats.transactions_pruned, 39 * 2);
    assert_eq!(stats.proof_bytes_freed, 39 * 2 * 1900);
    assert_eq!(stats.pruned_below, 40);
    assert_eq!(s.pruned_below().unwrap(), 40);

    // the state, every id, every Merkle root: exactly as before
    assert_eq!(s.state_digest().unwrap(), digest);
    for (i, b) in c.blocks.iter().enumerate() {
        let height = i as u64 + 1;
        let stored = s.get_block(height).unwrap().unwrap();
        assert_eq!(stored.is_pruned(), height < 40, "height {height}");
        assert_eq!(
            stored.into_full().is_some(),
            height >= 40,
            "height {height}"
        );
        assert_eq!(
            s.recompute_tx_root(height).unwrap(),
            Some(b.header.tx_root),
            "root at {height}"
        );
        for t in &b.transactions {
            let (h, st) = s
                .tx(&ids::tx_id(t).unwrap())
                .unwrap()
                .expect("the prefix row survives pruning");
            assert_eq!(h, height);
            assert_eq!(st.proof_data.is_some(), height >= 40);
            assert_eq!(
                ids::pruned_tx_id(&st.tx).unwrap(),
                ids::tx_id(t).unwrap(),
                "the id of the pruned form"
            );
        }
    }
    // idempotent, never backwards, and refuses to go past the tip
    assert_eq!(s.prune_below(40).unwrap().transactions_pruned, 0);
    assert_eq!(s.prune_below(10).unwrap().pruned_below, 40);
    assert_eq!(s.pruned_below().unwrap(), 40);
    assert_eq!(
        s.prune_below(62),
        Err(StoreError::PruneBeyondTip { tip: 60, asked: 62 })
    );
    // the recent-blocks policy
    let stats = s.prune_keeping(10).unwrap();
    assert_eq!(stats.pruned_below, 51);
    assert_eq!(stats.transactions_pruned, 11 * 2);
    assert_eq!(s.prune_keeping(1_000_000).unwrap().transactions_pruned, 0);
    assert_eq!(s.state_digest().unwrap(), digest);
}

#[test]
fn pruning_in_steps_gives_the_same_result_as_pruning_at_once() {
    let (a, b) = (TempDb::new("steps-a"), TempDb::new("steps-b"));
    let (mut ca, mut cb) = (Chain::new(&a, 9), Chain::new(&b, 9)); // the same seed: identical blocks
    ca.push_n(50);
    cb.push_n(50);
    assert_eq!(ca.blocks, cb.blocks);
    let once = ca.store.prune_below(37).unwrap();
    let stepped = cb.store.prune_below_in_steps(37, 5).unwrap();
    assert_eq!(once, stepped);
    assert_eq!(
        ca.store.state_digest().unwrap(),
        cb.store.state_digest().unwrap()
    );
    for h in 1..=50 {
        let (x, y) = (
            ca.store.get_block(h).unwrap().unwrap(),
            cb.store.get_block(h).unwrap().unwrap(),
        );
        assert_eq!(x, y, "height {h}");
        assert_eq!(x.is_pruned(), h < 37);
    }
    // already done, a step larger than the range, a zero step (treated as 1), and past the tip
    assert_eq!(
        cb.store
            .prune_below_in_steps(37, 5)
            .unwrap()
            .transactions_pruned,
        0
    );
    assert_eq!(
        cb.store
            .prune_below_in_steps(51, 1_000)
            .unwrap()
            .pruned_below,
        51
    );
    assert_eq!(
        ca.store.prune_below_in_steps(51, 0).unwrap().pruned_below,
        51
    );
    assert_eq!(
        cb.store.prune_below_in_steps(52, 5),
        Err(StoreError::PruneBeyondTip { tip: 50, asked: 52 })
    );
}

#[test]
fn a_reorganisation_can_go_back_through_pruned_blocks() {
    let db = TempDb::new("prune-pop");
    let mut c = Chain::new(&db, 5);
    let mut digests = vec![c.store.state_digest().unwrap()];
    for _ in 0..40 {
        c.push();
        digests.push(c.store.state_digest().unwrap());
    }
    c.store.prune_below(41).unwrap(); // everything, including the tip
    assert_eq!(c.store.pruned_below().unwrap(), 41);
    for height in (30..=40usize).rev() {
        let check = c.pop();
        assert!(
            check.popped.is_pruned(),
            "block {height} had lost its proofs"
        );
        assert_eq!(c.store.state_digest().unwrap(), digests[height - 1]);
        // pruned_below can never point past the tip: the next block at that height is a full one
        assert_eq!(c.store.pruned_below().unwrap(), height as u64);
    }
    // a different branch from height 29, with full proofs
    c.maker.next_key += 10_000;
    c.push_n(4);
    let (tip, _) = c.store.tip().unwrap();
    assert_eq!(tip, 33);
    assert!(!c.store.get_block(30).unwrap().unwrap().is_pruned());
    assert!(c.store.get_block(29).unwrap().unwrap().is_pruned());
    assert_eq!(c.store.state_digest().unwrap(), model_digest(&c.blocks));
    assert_eq!(
        c.store.recompute_tx_root(31).unwrap(),
        Some(c.blocks[30].header.tx_root)
    );
}

#[test]
fn the_database_survives_being_closed_and_is_bound_to_its_network() {
    let db = TempDb::new("reopen");
    let (digest, pruned_below, tip_id);
    {
        let mut c = Chain::new(&db, 6);
        c.push_n(25);
        c.store.prune_below(15).unwrap();
        digest = c.store.state_digest().unwrap();
        pruned_below = c.store.pruned_below().unwrap();
        tip_id = c.tip_id();
    }
    let s = db.open();
    assert_eq!(s.state_digest().unwrap(), digest);
    assert_eq!(s.pruned_below().unwrap(), pruned_below);
    let (h, tip) = s.tip().unwrap();
    assert_eq!((h, tip.block_id), (25, tip_id));
    assert!(
        s.get_block(14).unwrap().unwrap().is_pruned()
            && !s.get_block(15).unwrap().unwrap().is_pruned()
    );
    drop(s);
    // another network, or another proof of work, cannot open this file
    assert!(matches!(
        Store::open(&db.0, "some other network", POW),
        Err(StoreError::WrongChain)
    ));
    assert!(matches!(
        Store::open(&db.0, LABEL, PowKind::Matmul),
        Err(StoreError::WrongChain)
    ));
    // and refusing did not damage it
    assert_eq!(db.open().state_digest().unwrap(), digest);
}

#[test]
fn compaction_returns_the_space_of_pruned_proofs() {
    let db = TempDb::new("compact");
    let mut c = Chain::new(&db, 7);
    c.maker.proof_len = 1900;
    c.maker.txs_per_block = 40;
    c.push_n(150);
    let raw = c.store.file_size().unwrap();
    // One commit per block leaves copy-on-write slack, so the honest baseline is the FULL chain compacted.
    let mut store = c.store;
    store.compact().unwrap();
    let full = store.file_size().unwrap();
    let stats = store.prune_below(150).unwrap();
    let proofs = stats.proof_bytes_freed;
    let after_prune = store.file_size().unwrap();
    store.compact().unwrap();
    let after_compact = store.file_size().unwrap();
    println!(
        "150 blocks of 40 transactions ({} transactions): file as written {raw} bytes; compacted {full} bytes; \
         proofs {proofs} bytes ({:.0}% of that); after pruning, before compaction {after_prune}; after pruning and \
         compaction {after_compact} bytes = {:.0}% of the compacted full chain",
        stats.transactions_pruned,
        100.0 * proofs as f64 / full as f64,
        100.0 * after_compact as f64 / full as f64
    );
    // redb reuses freed pages but keeps the file the same size; compaction is what gives the space back
    assert!(
        after_prune >= after_compact,
        "compaction must not grow the file"
    );
    assert!(
        after_compact < full * 45 / 100,
        "pruning and compaction took the file from {full} to only {after_compact}"
    );
    assert!(
        after_compact > full / 10,
        "implausibly small: {after_compact}"
    );
    // and nothing that matters was lost
    assert_eq!(store.state_digest().unwrap(), model_digest(&c.blocks));
    assert_eq!(
        store.recompute_tx_root(75).unwrap(),
        Some(c.blocks[74].header.tx_root)
    );
}

/// A long random sequence of appends, reorganisations and prunings, checked against a plain in-memory model
/// after every step.
#[test]
fn a_random_history_of_appends_reorganisations_and_pruning_matches_the_model() {
    let db = TempDb::new("random");
    let mut c = Chain::new(&db, 8);
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut expected_pruned_below = 0u64;
    let mut ops = [0usize; 3];
    for step in 0..400 {
        match rng.below(10) {
            0..=5 => {
                ops[0] += 1;
                c.push();
            }
            6..=7 if !c.blocks.is_empty() => {
                ops[1] += 1;
                let n = 1 + rng.below(3.min(c.blocks.len() as u64)) as usize;
                for _ in 0..n {
                    let height = c.blocks.len() as u64;
                    let check = c.pop();
                    // the popped block still has its proofs exactly when it is at or above `pruned_below`
                    let expect =
                        (expected_pruned_below <= height).then(|| check.original.header.clone());
                    assert_eq!(check.popped.into_full().map(|b| b.header), expect);
                    expected_pruned_below = expected_pruned_below.min(height);
                }
            }
            _ => {
                ops[2] += 1;
                let tip = c.blocks.len() as u64;
                let below = rng.below(tip + 2);
                let s = c.store.prune_below(below).unwrap();
                expected_pruned_below = expected_pruned_below.max(below);
                assert_eq!(s.pruned_below, expected_pruned_below);
            }
        }
        let tip = c.blocks.len() as u64;
        assert_eq!(c.store.tip().unwrap().0, tip, "step {step}");
        assert_eq!(
            c.store.pruned_below().unwrap(),
            expected_pruned_below.min(tip + 1),
            "step {step}"
        );
        assert_eq!(
            c.store.state_digest().unwrap(),
            model_digest(&c.blocks),
            "state at step {step}"
        );
        if tip > 0 {
            let h = 1 + rng.below(tip);
            assert_eq!(
                c.store.recompute_tx_root(h).unwrap(),
                Some(c.blocks[h as usize - 1].header.tx_root),
                "step {step}"
            );
            let stored = c.store.get_block(h).unwrap().unwrap();
            assert_eq!(
                stored.is_pruned(),
                h < expected_pruned_below,
                "step {step}, height {h}"
            );
        }
    }
    println!(
        "random history: {} appends, {} reorganisations, {} prunings",
        ops[0], ops[1], ops[2]
    );
    assert!(
        ops.iter().all(|&n| n >= 20),
        "the random walk did not exercise every operation: {ops:?}"
    );
}
