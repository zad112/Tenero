//! The FCMP++ curve tree: the tree of every spendable output that a full-chain membership proof proves membership in
//! (`docs/FCMP_CARROT_PLAN.md` 4.3, milestone G3). **Consensus code we write** around the pinned crates' hashing; checked
//! against roots computed by Monero's own code (`tests/vectors/curve_tree_monero.json`). Unaudited.
//!
//! # The tree, defined by its ordered list of leaves
//! * A **leaf** is an output's six Selene scalars: the Weierstrass x and y of `O` (its key), `I` (its key-image
//!   generator, the unbiased `Hp²(O)`: every `gamma` output is a Carrot output) and `C` (its commitment).
//! * The first layer above the leaves holds **Selene** points: each hashes a chunk of `LEAF_CHUNK` (38) leaves, i.e. up to
//!   228 scalars. The next layer holds **Helios** points, each hashing a chunk of 18 Selene points (by their x
//!   coordinates, which are Helios scalars); the next Selene points over chunks of 38 Helios points; and so on.
//! * A chunk's hash is `init + sum_j g_j * child_j` (the pinned crate's `hash_grow`), with the curve's own generators.
//! * The tree has as many layers as it takes to reach one element: the **root**. One layer (a Selene root) holds up to
//!   38 outputs, two up to 684, three up to 25,992, four up to 467,856.
//!
//! The hash is linear, so growing it block by block (only the changed tail) gives exactly the root of hashing everything
//! from scratch; a trim recomputes the tail chunk of each layer. Both are tested against a from-scratch build.

use std::sync::LazyLock;

use ciphersuite::group::ff::Field;
use ciphersuite::Ciphersuite;
use dalek_ff_group::{Ed25519, EdwardsPoint};
use ec_divisors::DivisorCurve;
use full_chain_membership_proofs::tree::hash_grow;
use generalized_bulletproofs::Generators;
use helioselene::{Helios, Selene};
use monero_fcmp_plus_plus::fcmps::{Path, TreeRoot, LAYER_ONE_LEN, LAYER_TWO_LEN};
use monero_fcmp_plus_plus::{Curves, Output, HELIOS_FCMP_GENERATORS, SELENE_FCMP_GENERATORS};
use monero_fcmp_plus_plus_generators::{HELIOS_HASH_INIT, SELENE_HASH_INIT};

/// Outputs per chunk of leaves (Monero's `SELENE_CHUNK_WIDTH`).
pub const LEAF_CHUNK: usize = LAYER_ONE_LEN;
/// Children per Selene parent above the first layer (also 38) and per Helios parent (18).
pub const SELENE_WIDTH: usize = LAYER_ONE_LEN;
pub const HELIOS_WIDTH: usize = LAYER_TWO_LEN;
/// Scalars per leaf.
pub const LEAF_SCALARS: usize = 6;

type SeleneF = <Selene as Ciphersuite>::F;
type SeleneG = <Selene as Ciphersuite>::G;
type HeliosF = <Helios as Ciphersuite>::F;
type HeliosG = <Helios as Ciphersuite>::G;

static SELENE_GENS: LazyLock<&'static Generators<Selene>> =
    LazyLock::new(|| &SELENE_FCMP_GENERATORS.generators);
static HELIOS_GENS: LazyLock<&'static Generators<Helios>> =
    LazyLock::new(|| &HELIOS_FCMP_GENERATORS.generators);

/// An output as the tree sees it: its key `O`, key-image generator `I` and commitment `C`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Leaf {
    pub output: Output,
}

impl Leaf {
    /// The leaf of a `gamma` output: `O` and `C` as stored, and `I = Hp²(O)`. `None` unless both are canonical points of
    /// prime order other than the identity ([`strict_point`]: what consensus requires of every output).
    pub fn from_output(onetime_address: &[u8; 32], commitment: &[u8; 32]) -> Option<Leaf> {
        let o = strict_point(onetime_address)?;
        let c = strict_point(commitment)?;
        let i = monero_ed25519::Point::hash(*onetime_address).into();
        let output = Output::new(EdwardsPoint(o), EdwardsPoint(i), EdwardsPoint(c)).ok()?;
        Some(Leaf { output })
    }

    /// The six Selene scalars `{O.x, O.y, I.x, I.y, C.x, C.y}`.
    pub fn scalars(&self) -> [SeleneF; LEAF_SCALARS] {
        let xy = |p: EdwardsPoint| {
            <Ed25519 as Ciphersuite>::G::to_xy(p).expect("an output point is not the identity")
        };
        let (ox, oy) = xy(self.output.O());
        let (ix, iy) = xy(self.output.I());
        let (cx, cy) = xy(self.output.C());
        [ox, oy, ix, iy, cx, cy]
    }
}

/// How many layers a tree of `n` leaves has (0 for an empty tree).
pub fn n_layers(n: u64) -> usize {
    if n == 0 {
        return 0;
    }
    let mut layers = 1;
    let mut count = n.div_ceil(LEAF_CHUNK as u64);
    while count > 1 {
        // the layer just made is Selene when `layers` is odd, so the next is Helios (width 18), and the other way round
        let width = if layers % 2 == 1 {
            HELIOS_WIDTH
        } else {
            SELENE_WIDTH
        } as u64;
        count = count.div_ceil(width);
        layers += 1;
    }
    layers
}

fn selene_x(p: &SeleneG) -> HeliosF {
    <Selene as Ciphersuite>::G::to_xy(*p)
        .expect("a tree hash is not the identity")
        .0
}

fn helios_x(p: &HeliosG) -> SeleneF {
    <Helios as Ciphersuite>::G::to_xy(*p)
        .expect("a tree hash is not the identity")
        .0
}

fn hash_new<C: Ciphersuite>(gens: &Generators<C>, init: C::G, children: &[C::F]) -> C::G {
    hash_grow(gens, init, 0, C::F::ZERO, children).expect("a chunk fits the generators")
}

/// Grows one parent layer. `children(i)` is child `i` as a scalar; the children from `start` on are new or changed
/// (`old_start` is the old value of child `start` when it existed before, else zero); there are `n_children` now.
/// Returns the index of the first parent that changed and, if that parent existed, its old value.
#[allow(clippy::too_many_arguments)]
fn grow_layer<C: Ciphersuite>(
    gens: &Generators<C>,
    init: C::G,
    width: usize,
    parents: &mut Vec<C::G>,
    children: impl Fn(usize) -> C::F,
    start: usize,
    old_start: Option<C::F>,
    n_children: usize,
) -> (usize, Option<C::G>) {
    let first = start / width;
    let old_first = parents.get(first).copied();
    let mut chunk = first;
    while chunk * width < n_children {
        let chunk_start = chunk * width;
        let chunk_end = (chunk_start + width).min(n_children);
        if let Some(existing) = parents.get(chunk).copied() {
            // only the first changed chunk can already exist: grow it from `start`
            debug_assert_eq!(chunk, first);
            let new: Vec<C::F> = (start..chunk_end).map(&children).collect();
            let offset = start - chunk_start;
            parents[chunk] = hash_grow(
                gens,
                existing,
                offset,
                old_start.unwrap_or(C::F::ZERO),
                &new,
            )
            .expect("a chunk fits the generators");
        } else {
            let all: Vec<C::F> = (chunk_start..chunk_end).map(&children).collect();
            parents.push(hash_new(gens, init, &all));
        }
        chunk += 1;
    }
    (first, old_first)
}

/// Recomputes the last parent of a layer from its children, after the children were cut to `n_children`.
fn trim_layer<C: Ciphersuite>(
    gens: &Generators<C>,
    init: C::G,
    width: usize,
    parents: &mut Vec<C::G>,
    children: impl Fn(usize) -> C::F,
    n_children: usize,
) {
    let n_parents = n_children.div_ceil(width);
    parents.truncate(n_parents);
    if n_parents > 0 {
        let start = (n_parents - 1) * width;
        let all: Vec<C::F> = (start..n_children).map(children).collect();
        parents[n_parents - 1] = hash_new(gens, init, &all);
    }
}

/// The tree's hashes (every layer above the leaves) and its number of leaves. The leaves themselves are not kept: they
/// are the outputs, which the caller reads back when a path or a trim needs them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CurveTree {
    n_leaves: u64,
    /// `selene[0]` is the layer above the leaves, then `helios[0]`, `selene[1]`, ...
    selene: Vec<Vec<SeleneG>>,
    helios: Vec<Vec<HeliosG>>,
}

impl CurveTree {
    pub fn new() -> CurveTree {
        CurveTree::default()
    }

    pub fn n_leaves(&self) -> u64 {
        self.n_leaves
    }

    pub fn n_layers(&self) -> usize {
        self.selene.len() + self.helios.len()
    }

    /// The root, or `None` for an empty tree.
    pub fn root(&self) -> Option<TreeRoot<Selene, Helios>> {
        match self.n_layers() {
            0 => None,
            l if l % 2 == 1 => Some(TreeRoot::C1(self.selene.last()?[0])),
            _ => Some(TreeRoot::C2(self.helios.last()?[0])),
        }
    }

    /// The root's 32 bytes (a Selene point for an odd number of layers, a Helios point for an even one).
    pub fn root_bytes(&self) -> Option<[u8; 32]> {
        use ciphersuite::group::GroupEncoding;
        Some(match self.root()? {
            TreeRoot::C1(p) => p.to_bytes(),
            TreeRoot::C2(p) => p.to_bytes(),
        })
    }

    /// Adds leaves at the end (one block's outputs that became spendable, in order).
    pub fn grow(&mut self, new_leaves: &[Leaf]) {
        if new_leaves.is_empty() {
            return;
        }
        let old_n = self.n_leaves as usize;
        let new_n = old_n + new_leaves.len();
        let target = n_layers(new_n as u64);

        // the layer above the leaves: leaves are only ever appended, so the old value of a changed child is zero
        let scalars: Vec<SeleneF> = new_leaves.iter().flat_map(|l| l.scalars()).collect();
        if self.selene.is_empty() {
            self.selene.push(Vec::new());
        }
        let base = old_n * LEAF_SCALARS;
        let (mut start, mut old_selene) = grow_layer(
            &SELENE_GENS,
            *SELENE_HASH_INIT,
            LEAF_CHUNK * LEAF_SCALARS,
            &mut self.selene[0],
            |i| scalars[i - base],
            base,
            None,
            new_n * LEAF_SCALARS,
        );
        let mut old_helios: Option<HeliosG> = None;

        // then alternately Helios and Selene layers until the root
        for layer in 1..target {
            if layer % 2 == 1 {
                let h = layer / 2;
                if self.helios.len() == h {
                    self.helios.push(Vec::new());
                }
                let children = &self.selene[h];
                let n_children = children.len();
                let old = old_selene.map(|p| selene_x(&p));
                let (first, old_parent) = grow_layer(
                    &HELIOS_GENS,
                    *HELIOS_HASH_INIT,
                    HELIOS_WIDTH,
                    &mut self.helios[h],
                    |i| selene_x(&children[i]),
                    start,
                    old,
                    n_children,
                );
                start = first;
                old_helios = old_parent;
            } else {
                let s = layer / 2;
                if self.selene.len() == s {
                    self.selene.push(Vec::new());
                }
                let children = &self.helios[s - 1];
                let n_children = children.len();
                let old = old_helios.map(|p| helios_x(&p));
                let (first, old_parent) = grow_layer(
                    &SELENE_GENS,
                    *SELENE_HASH_INIT,
                    SELENE_WIDTH,
                    &mut self.selene[s],
                    |i| helios_x(&children[i]),
                    start,
                    old,
                    n_children,
                );
                start = first;
                old_selene = old_parent;
            }
        }
        self.n_leaves = new_n as u64;
        debug_assert_eq!(self.n_layers(), target);
    }

    /// Cuts the tree back to its first `new_n` leaves (undoing blocks). `leaf(i)` gives leaf `i`: only the last chunk
    /// of leaves is read.
    pub fn trim(&mut self, new_n: u64, leaf: impl Fn(u64) -> Leaf) {
        assert!(new_n <= self.n_leaves, "a trim cannot grow the tree");
        if new_n == self.n_leaves {
            return;
        }
        let target = n_layers(new_n);
        self.selene.truncate(target.div_ceil(2));
        self.helios.truncate(target / 2);
        self.n_leaves = new_n;
        if target == 0 {
            return;
        }
        let n = new_n as usize;
        let last_chunk = (n - 1) / LEAF_CHUNK * LEAF_CHUNK;
        let tail: Vec<SeleneF> = (last_chunk..n)
            .flat_map(|i| leaf(i as u64).scalars())
            .collect();
        trim_layer(
            &SELENE_GENS,
            *SELENE_HASH_INIT,
            LEAF_CHUNK * LEAF_SCALARS,
            &mut self.selene[0],
            |i| tail[i - last_chunk * LEAF_SCALARS],
            n * LEAF_SCALARS,
        );
        for layer in 1..target {
            if layer % 2 == 1 {
                let h = layer / 2;
                let children = &self.selene[h];
                let n_children = children.len();
                trim_layer(
                    &HELIOS_GENS,
                    *HELIOS_HASH_INIT,
                    HELIOS_WIDTH,
                    &mut self.helios[h],
                    |i| selene_x(&children[i]),
                    n_children,
                );
            } else {
                let s = layer / 2;
                let children = self.helios[s - 1].clone();
                trim_layer(
                    &SELENE_GENS,
                    *SELENE_HASH_INIT,
                    SELENE_WIDTH,
                    &mut self.selene[s],
                    |i| helios_x(&children[i]),
                    children.len(),
                );
            }
        }
    }

    /// The path of leaf `index` for a proof: the chunk of leaves it is in, then the chunk of each layer above that
    /// holds its branch, up to the root's children. `leaf(i)` gives leaf `i` (only the leaf's chunk is read).
    /// As in Monero's `path_for_proof`, a layer's chunk is padded with zeros to its full width (a zero child adds
    /// nothing to the hash; the proof's circuit has fixed widths); the chunk of leaves is not padded.
    pub fn path(&self, index: u64, leaf: impl Fn(u64) -> Leaf) -> Option<Path<Curves>> {
        if index >= self.n_leaves {
            return None;
        }
        let n = self.n_leaves as usize;
        let i = index as usize;
        let chunk_start = i / LEAF_CHUNK * LEAF_CHUNK;
        let chunk_end = (chunk_start + LEAF_CHUNK).min(n);
        let leaves: Vec<Output> = (chunk_start..chunk_end)
            .map(|j| leaf(j as u64).output)
            .collect();
        let output = leaves[i - chunk_start];
        let mut curve_2_layers = vec![];
        let mut curve_1_layers = vec![];
        let mut pos = i / LEAF_CHUNK; // our branch's index in the layer being walked
        for layer in 1..self.n_layers() {
            if layer % 2 == 1 {
                // children are Selene points (layer `selene[h]`), hashed into a Helios parent
                let children = &self.selene[layer / 2];
                let start = pos / HELIOS_WIDTH * HELIOS_WIDTH;
                let end = (start + HELIOS_WIDTH).min(children.len());
                let mut chunk: Vec<HeliosF> = children[start..end].iter().map(selene_x).collect();
                chunk.resize(HELIOS_WIDTH, HeliosF::ZERO);
                curve_2_layers.push(chunk);
                pos /= HELIOS_WIDTH;
            } else {
                let children = &self.helios[layer / 2 - 1];
                let start = pos / SELENE_WIDTH * SELENE_WIDTH;
                let end = (start + SELENE_WIDTH).min(children.len());
                let mut chunk: Vec<SeleneF> = children[start..end].iter().map(helios_x).collect();
                chunk.resize(SELENE_WIDTH, SeleneF::ZERO);
                curve_1_layers.push(chunk);
                pos /= SELENE_WIDTH;
            }
        }
        Some(Path {
            output,
            leaves,
            curve_2_layers,
            curve_1_layers,
        })
    }

    /// The path of leaf `index` as bytes ([`PathBytes`]): what a node hands a wallet so it can prove a spend (over a socket
    /// too). `leaf(i)` gives leaf `i`'s one-time address and commitment.
    pub fn path_bytes(
        &self,
        index: u64,
        leaf: impl Fn(u64) -> ([u8; 32], [u8; 32]),
    ) -> Option<PathBytes> {
        if index >= self.n_leaves {
            return None;
        }
        let n = self.n_leaves as usize;
        let i = index as usize;
        let chunk_start = i / LEAF_CHUNK * LEAF_CHUNK;
        let chunk_end = (chunk_start + LEAF_CHUNK).min(n);
        let leaves = (chunk_start..chunk_end).map(|j| leaf(j as u64)).collect();
        let mut layers = vec![];
        let mut pos = i / LEAF_CHUNK;
        for layer in 1..self.n_layers() {
            let (len, width) = if layer % 2 == 1 {
                (self.selene[layer / 2].len(), HELIOS_WIDTH)
            } else {
                (self.helios[layer / 2 - 1].len(), SELENE_WIDTH)
            };
            let start = pos / width * width;
            let end = (start + width).min(len);
            layers.push(
                (start..end)
                    .map(|k| self.element_bytes(layer - 1, k).expect("in range"))
                    .collect(),
            );
            pos /= width;
        }
        Some(PathBytes {
            position: index,
            leaves,
            layers,
        })
    }

    /// The number of elements in each layer above the leaves, from the bottom (Selene, Helios, Selene, ...).
    pub fn layer_lens(&self) -> Vec<usize> {
        (0..self.n_layers())
            .map(|l| {
                if l % 2 == 0 {
                    self.selene[l / 2].len()
                } else {
                    self.helios[l / 2].len()
                }
            })
            .collect()
    }

    /// Element `index` of layer `layer` as its 32 bytes (a Selene point in an even layer, a Helios point in an odd one).
    pub fn element_bytes(&self, layer: usize, index: usize) -> Option<[u8; 32]> {
        use ciphersuite::group::GroupEncoding;
        if layer.is_multiple_of(2) {
            Some(self.selene.get(layer / 2)?.get(index)?.to_bytes())
        } else {
            Some(self.helios.get(layer / 2)?.get(index)?.to_bytes())
        }
    }

    /// Rebuilds a tree from its number of leaves and every element of every layer, as [`Self::element_bytes`] gave
    /// them. `None` if a point does not decode or the shape is not that of a tree of `n_leaves` leaves.
    pub fn from_layers(n_leaves: u64, layers: Vec<Vec<[u8; 32]>>) -> Option<CurveTree> {
        use ciphersuite::group::GroupEncoding;
        if layers.len() != n_layers(n_leaves) {
            return None;
        }
        let mut t = CurveTree {
            n_leaves,
            selene: vec![],
            helios: vec![],
        };
        for (l, layer) in layers.into_iter().enumerate() {
            if l % 2 == 0 {
                let pts = layer
                    .iter()
                    .map(|b| Option::from(SeleneG::from_bytes(b)))
                    .collect::<Option<Vec<_>>>()?;
                t.selene.push(pts);
            } else {
                let pts = layer
                    .iter()
                    .map(|b| Option::from(HeliosG::from_bytes(b)))
                    .collect::<Option<Vec<_>>>()?;
                t.helios.push(pts);
            }
        }
        // each layer has the number of elements a tree of n_leaves has
        let mut want = n_leaves.div_ceil(LEAF_CHUNK as u64) as usize;
        for (l, len) in t.layer_lens().into_iter().enumerate() {
            if len != want {
                return None;
            }
            want = want.div_ceil(if l % 2 == 0 {
                HELIOS_WIDTH
            } else {
                SELENE_WIDTH
            });
        }
        Some(t)
    }

    /// Builds a tree from all its leaves at once, the plain definition (for tests and audits).
    pub fn from_scratch(leaves: &[Leaf]) -> CurveTree {
        let mut t = CurveTree::new();
        if leaves.is_empty() {
            return t;
        }
        let scalars: Vec<SeleneF> = leaves.iter().flat_map(|l| l.scalars()).collect();
        let first: Vec<SeleneG> = scalars
            .chunks(LEAF_CHUNK * LEAF_SCALARS)
            .map(|c| hash_new(&SELENE_GENS, *SELENE_HASH_INIT, c))
            .collect();
        t.selene.push(first);
        loop {
            let top_len = if t.n_layers() % 2 == 1 {
                t.selene.last().unwrap().len()
            } else {
                t.helios.last().unwrap().len()
            };
            if top_len == 1 {
                break;
            }
            if t.n_layers() % 2 == 1 {
                let xs: Vec<HeliosF> = t.selene.last().unwrap().iter().map(selene_x).collect();
                let next = xs
                    .chunks(HELIOS_WIDTH)
                    .map(|c| hash_new(&HELIOS_GENS, *HELIOS_HASH_INIT, c))
                    .collect();
                t.helios.push(next);
            } else {
                let xs: Vec<SeleneF> = t.helios.last().unwrap().iter().map(helios_x).collect();
                let next = xs
                    .chunks(SELENE_WIDTH)
                    .map(|c| hash_new(&SELENE_GENS, *SELENE_HASH_INIT, c))
                    .collect();
                t.selene.push(next);
            }
        }
        t.n_leaves = leaves.len() as u64;
        t
    }
}

/// A point consensus accepts in an output, a key image or a pseudo-output: canonical, of prime order, not the identity.
pub fn strict_point(bytes: &[u8; 32]) -> Option<curve25519_dalek::EdwardsPoint> {
    use curve25519_dalek::traits::IsIdentity;
    let p: curve25519_dalek::EdwardsPoint = monero_ed25519::CompressedPoint::from(*bytes)
        .decompress()?
        .into();
    (p.is_torsion_free() && !IsIdentity::is_identity(&p)).then_some(p)
}

/// Monero's hash-to-point of 32 bytes, compressed: a point of prime order that no one knows the discrete log of. For
/// tests (valid outputs, key images and keys without secrets) and as `I = Hp(O)`.
pub fn hash_to_point(bytes: [u8; 32]) -> [u8; 32] {
    monero_ed25519::Point::hash(bytes).compress().to_bytes()
}

/// A leaf's path in bytes: its position, the one-time address and commitment of every leaf in its chunk, and the chunk
/// of each layer above that holds its branch (as point bytes, not padded), up to the root's children. Nothing in it is
/// trusted: [`path_from_bytes`] refuses what does not decode, and a path that is not the tree's makes a proof that fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathBytes {
    pub position: u64,
    pub leaves: Vec<([u8; 32], [u8; 32])>,
    pub layers: Vec<Vec<[u8; 32]>>,
}

/// The prover's path from [`PathBytes`]. `None` if a point does not decode, a chunk is empty or wider than its layer
/// allows, or the position is not in the leaf chunk.
pub fn path_from_bytes(p: &PathBytes) -> Option<Path<Curves>> {
    use ciphersuite::group::{Group, GroupEncoding};
    if p.leaves.is_empty() || p.leaves.len() > LEAF_CHUNK {
        return None;
    }
    let in_chunk = (p.position % LEAF_CHUNK as u64) as usize;
    let leaves: Vec<Output> = p
        .leaves
        .iter()
        .map(|(o, c)| Leaf::from_output(o, c).map(|l| l.output))
        .collect::<Option<_>>()?;
    let output = *leaves.get(in_chunk)?;
    let mut curve_2_layers = vec![];
    let mut curve_1_layers = vec![];
    for (k, chunk) in p.layers.iter().enumerate() {
        let layer = k + 1;
        if layer % 2 == 1 {
            if chunk.is_empty() || chunk.len() > HELIOS_WIDTH {
                return None;
            }
            let mut xs = chunk
                .iter()
                .map(|b| {
                    let g: SeleneG = Option::from(SeleneG::from_bytes(b))?;
                    (!bool::from(g.is_identity())).then(|| selene_x(&g))
                })
                .collect::<Option<Vec<HeliosF>>>()?;
            xs.resize(HELIOS_WIDTH, HeliosF::ZERO);
            curve_2_layers.push(xs);
        } else {
            if chunk.is_empty() || chunk.len() > SELENE_WIDTH {
                return None;
            }
            let mut xs = chunk
                .iter()
                .map(|b| {
                    let g: HeliosG = Option::from(HeliosG::from_bytes(b))?;
                    (!bool::from(g.is_identity())).then(|| helios_x(&g))
                })
                .collect::<Option<Vec<SeleneF>>>()?;
            xs.resize(SELENE_WIDTH, SeleneF::ZERO);
            curve_1_layers.push(xs);
        }
    }
    Some(Path {
        output,
        leaves,
        curve_2_layers,
        curve_1_layers,
    })
}

/// The commitment of a coinbase output, whose amount is public: `1*G + amount*H` (Carrot 4.1, `docs/CONSENSUS_V2.md` 15.6).
pub fn coinbase_commitment(amount: u64) -> [u8; 32] {
    monero_ed25519::Commitment::new(monero_ed25519::Scalar::ONE, amount)
        .commit()
        .compress()
        .to_bytes()
}

/// A root from its 32 bytes, for a tree of `layers` layers: a Selene point when `layers` is odd, a Helios point when it
/// is even. `None` if the bytes are not such a point or `layers` is 0.
pub fn root_from_bytes(layers: usize, bytes: &[u8; 32]) -> Option<TreeRoot<Selene, Helios>> {
    use ciphersuite::group::GroupEncoding;
    match layers {
        0 => None,
        l if l % 2 == 1 => Option::from(SeleneG::from_bytes(bytes)).map(TreeRoot::C1),
        _ => Option::from(HeliosG::from_bytes(bytes)).map(TreeRoot::C2),
    }
}
