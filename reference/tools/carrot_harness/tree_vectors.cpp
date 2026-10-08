// Tenero: makes tests/vectors/curve_tree_monero.json with MONERO'S OWN curve-tree code (stressnet v0.19.0.0-beta.3.0):
// the production CurveTrees::get_tree_extension grows a tree block by block, and after every block Monero's own test audit
// (re-hashing every layer from scratch) must pass; the root after each block is recorded. Built in a Monero checkout by
// run.sh (WSL), never part of Tenero's build. crates/tenero-crypto/tests/curve_tree_monero.rs must reproduce every root.
//
// The outputs are derived from their index, so the file need not hold them: output i has key o_i G and commitment c_i G
// with o_i = Keccak256("tenero curve tree vector O" || i as 8 bytes LE) mod l, c_i likewise with "... C ...". They are
// Carrot outputs (CarrotOutputPairV1: no torsion clearing, the unbiased key-image generator), as every output on gamma is.
//
// CurveTreesGlobalTree below (the class, validate_layer, extend_tree, get_last_hashes and audit_tree) is copied unchanged from Monero's
// tests/unit_tests/curve_trees.{h,cpp} at the same tag: Copyright (c) 2014-2024, The Monero Project, BSD-3-Clause.

#include <cstdio>
#include <string>
#include <vector>

#include "crypto/crypto.h"
#include "crypto/hash.h"
#include "fcmp_pp/curve_trees.h"
#include "fcmp_pp/fcmp_pp_crypto.h"
#include "misc_log_ex.h"
#include "ringct/rctOps.h"

using namespace fcmp_pp::curve_trees;
using Selene = fcmp_pp::curve_trees::Selene;
using Helios = fcmp_pp::curve_trees::Helios;

class CurveTreesGlobalTree
{
public:
    CurveTreesGlobalTree(CurveTreesV1 &curve_trees): m_curve_trees(curve_trees) {};
    template<typename C>
    using Layer = std::vector<typename C::Point>;
    struct Tree final
    {
        std::vector<fcmp_pp::OutputPair> leaves;
        std::vector<Layer<Selene>> c1_layers;
        std::vector<Layer<Helios>> c2_layers;
    };
    bool audit_tree(const std::size_t expected_n_leaf_tuples) const;
    void extend_tree(const CurveTreesV1::TreeExtension &tree_extension);
    CurveTreesV1::LastHashes get_last_hashes() const;
    CurveTreesV1 &m_curve_trees;
    Tree m_tree = Tree{};
};
//----------------------------------------------------------------------------------------------------------------------
template<typename C>
static bool validate_layer(const std::unique_ptr<C> &curve,
    const CurveTreesGlobalTree::Layer<C> &parents,
    const std::vector<typename C::Scalar> &child_scalars,
    const std::size_t max_chunk_size)
{
    // Hash chunk of children scalars, then see if the hash matches up to respective parent
    std::size_t chunk_start_idx = 0;
    for (std::size_t i = 0; i < parents.size(); ++i)
    {
        CHECK_AND_ASSERT_MES(child_scalars.size() > chunk_start_idx, false, "chunk start too high");
        const std::size_t chunk_size = std::min(child_scalars.size() - chunk_start_idx, max_chunk_size);
        CHECK_AND_ASSERT_MES(child_scalars.size() >= (chunk_start_idx + chunk_size), false, "chunk size too large");

        const typename C::Point &parent = parents[i];

        const auto chunk_start = child_scalars.data() + chunk_start_idx;
        const typename C::Chunk chunk{chunk_start, chunk_size};

        for (std::size_t i = 0; i < chunk_size; ++i)
            MDEBUG("Hashing " << curve->to_string(chunk_start[i]));

        const typename C::Point chunk_hash = fcmp_pp::curve_trees::get_new_parent(curve, chunk);

        MDEBUG("chunk_start_idx: " << chunk_start_idx << " , chunk_size: " << chunk_size << " , chunk_hash: " << curve->to_string(chunk_hash));

        const auto actual = curve->to_string(parent);
        const auto expected = curve->to_string(chunk_hash);
        CHECK_AND_ASSERT_MES(actual == expected, false, "unexpected hash");

        chunk_start_idx += chunk_size;
    }

    CHECK_AND_ASSERT_THROW_MES(chunk_start_idx == child_scalars.size(), "unexpected ending chunk start idx");

    return true;
}
//----------------------------------------------------------------------------------------------------------------------
bool CurveTreesGlobalTree::audit_tree(const std::size_t expected_n_leaf_tuples) const
{
    MDEBUG("Auditing global tree");

    auto leaves = m_tree.leaves;
    const auto &c1_layers = m_tree.c1_layers;
    const auto &c2_layers = m_tree.c2_layers;

    CHECK_AND_ASSERT_MES(leaves.size() == expected_n_leaf_tuples, false, "unexpected num leaves");

    if (leaves.empty())
    {
        CHECK_AND_ASSERT_MES(c1_layers.empty() && c2_layers.empty(), false, "expected empty tree");
        return true;
    }

    CHECK_AND_ASSERT_MES(!c1_layers.empty(), false, "must have at least 1 c1 layer in tree");
    CHECK_AND_ASSERT_MES(c1_layers.size() == c2_layers.size() || c1_layers.size() == (c2_layers.size() + 1),
        false, "unexpected mismatch of c1 and c2 layers");

    const std::size_t n_layers = c1_layers.size() + c2_layers.size();
    CHECK_AND_ASSERT_MES(n_layers == m_curve_trees.n_layers(leaves.size()), false, "unexpected n_layers");

    // Verify root has 1 member in it
    const bool c1_is_root = c1_layers.size() > c2_layers.size();
    CHECK_AND_ASSERT_MES(c1_is_root ? c1_layers.back().size() == 1 : c2_layers.back().size() == 1, false,
        "root must have 1 member in it");

    // Iterate from root down to layer above leaves, and check hashes match up correctly
    bool parent_is_c1 = c1_is_root;
    std::size_t c1_idx = c1_layers.size() - 1;
    std::size_t c2_idx = c2_layers.empty() ? 0 : (c2_layers.size() - 1);
    for (std::size_t i = 1; i < n_layers; ++i)
    {
        // TODO: implement templated function for below if statement
        if (parent_is_c1)
        {
            MDEBUG("Validating parent c1 layer " << c1_idx << " , child c2 layer " << c2_idx);

            CHECK_AND_ASSERT_THROW_MES(c1_idx < c1_layers.size(), "unexpected c1_idx");
            CHECK_AND_ASSERT_THROW_MES(c2_idx < c2_layers.size(), "unexpected c2_idx");

            const Layer<Selene> &parents  = c1_layers[c1_idx];
            const Layer<Helios> &children = c2_layers[c2_idx];

            CHECK_AND_ASSERT_MES(!parents.empty(), false, "no parents at c1_idx " + std::to_string(c1_idx));
            CHECK_AND_ASSERT_MES(!children.empty(), false, "no children at c2_idx " + std::to_string(c2_idx));

            std::vector<Selene::Scalar> child_scalars;
            fcmp_pp::tower_cycle::extend_scalars_from_cycle_points<Helios, Selene>(m_curve_trees.m_c2,
                children,
                child_scalars);

            const bool valid = validate_layer<Selene>(
                m_curve_trees.m_c1,
                parents,
                child_scalars,
                m_curve_trees.m_c1_width);

            CHECK_AND_ASSERT_MES(valid, false, "failed to validate c1_idx " + std::to_string(c1_idx));

            --c1_idx;
        }
        else
        {
            MDEBUG("Validating parent c2 layer " << c2_idx << " , child c1 layer " << c1_idx);

            CHECK_AND_ASSERT_THROW_MES(c2_idx < c2_layers.size(), "unexpected c2_idx");
            CHECK_AND_ASSERT_THROW_MES(c1_idx < c1_layers.size(), "unexpected c1_idx");

            const Layer<Helios> &parents  = c2_layers[c2_idx];
            const Layer<Selene> &children = c1_layers[c1_idx];

            CHECK_AND_ASSERT_MES(!parents.empty(), false, "no parents at c2_idx " + std::to_string(c2_idx));
            CHECK_AND_ASSERT_MES(!children.empty(), false, "no children at c1_idx " + std::to_string(c1_idx));

            std::vector<Helios::Scalar> child_scalars;
            fcmp_pp::tower_cycle::extend_scalars_from_cycle_points<Selene, Helios>(m_curve_trees.m_c1,
                children,
                child_scalars);

            const bool valid = validate_layer<Helios>(m_curve_trees.m_c2,
                parents,
                child_scalars,
                m_curve_trees.m_c2_width);

            CHECK_AND_ASSERT_MES(valid, false, "failed to validate c2_idx " + std::to_string(c2_idx));

            --c2_idx;
        }

        parent_is_c1 = !parent_is_c1;
    }

    MDEBUG("Validating leaves");

    // Convert output pairs to leaf tuples
    std::vector<CurveTreesV1::LeafTuple> leaf_tuples;
    leaf_tuples.reserve(leaves.size());
    for (const auto &leaf : leaves)
    {
        auto leaf_tuple = m_curve_trees.leaf_tuple(leaf);
        leaf_tuples.emplace_back(std::move(leaf_tuple));
    }

    // Now validate leaves
    return validate_layer<Selene>(m_curve_trees.m_c1,
        c1_layers[0],
        m_curve_trees.flatten_leaves(std::move(leaf_tuples)),
        m_curve_trees.m_leaf_layer_chunk_width);
}

void CurveTreesGlobalTree::extend_tree(const CurveTreesV1::TreeExtension &tree_extension)
{
    // Add the leaves
    CHECK_AND_ASSERT_THROW_MES(m_tree.leaves.size() == tree_extension.leaves.start_leaf_tuple_idx,
        "unexpected leaf start idx");

    m_tree.leaves.reserve(m_tree.leaves.size() + tree_extension.leaves.tuples.size());
    for (const auto &o : tree_extension.leaves.tuples)
    {
        m_tree.leaves.emplace_back(o.output_pair);
    }

    // Add the layers
    const auto &c1_extensions = tree_extension.c1_layer_extensions;
    const auto &c2_extensions = tree_extension.c2_layer_extensions;
    CHECK_AND_ASSERT_THROW_MES(!c1_extensions.empty(), "empty c1 extensions");

    bool parent_is_c1 = true;
    std::size_t c1_idx = 0, c2_idx = 0;
    for (std::size_t i = 0; i < (c1_extensions.size() + c2_extensions.size()); ++i)
    {
        // TODO: template below if statement
        if (parent_is_c1)
        {
            CHECK_AND_ASSERT_THROW_MES(c1_idx < c1_extensions.size(), "unexpected c1 layer extension");
            const fcmp_pp::curve_trees::LayerExtension<Selene> &c1_ext = c1_extensions[c1_idx];

            CHECK_AND_ASSERT_THROW_MES(!c1_ext.hashes.empty(), "empty c1 layer extension");

            CHECK_AND_ASSERT_THROW_MES(c1_idx <= m_tree.c1_layers.size(), "missing c1 layer");
            if (m_tree.c1_layers.size() == c1_idx)
                m_tree.c1_layers.emplace_back(Layer<Selene>{});

            auto &c1_inout = m_tree.c1_layers[c1_idx];

            const bool started_after_tip = (c1_inout.size() == c1_ext.start_idx);
            const bool started_at_tip    = (c1_inout.size() == (c1_ext.start_idx + 1));
            CHECK_AND_ASSERT_THROW_MES(started_after_tip || started_at_tip, "unexpected c1 layer start");

            // We updated the last hash
            if (started_at_tip)
            {
                CHECK_AND_ASSERT_THROW_MES(c1_ext.update_existing_last_hash, "expect to be updating last hash");
                c1_inout.back() = c1_ext.hashes.front();
            }
            else
            {
                CHECK_AND_ASSERT_THROW_MES(!c1_ext.update_existing_last_hash, "unexpected last hash update");
            }

            for (std::size_t i = started_at_tip ? 1 : 0; i < c1_ext.hashes.size(); ++i)
                c1_inout.emplace_back(c1_ext.hashes[i]);

            ++c1_idx;
        }
        else
        {
            CHECK_AND_ASSERT_THROW_MES(c2_idx < c2_extensions.size(), "unexpected c2 layer extension");
            const fcmp_pp::curve_trees::LayerExtension<Helios> &c2_ext = c2_extensions[c2_idx];

            CHECK_AND_ASSERT_THROW_MES(!c2_ext.hashes.empty(), "empty c2 layer extension");

            CHECK_AND_ASSERT_THROW_MES(c2_idx <= m_tree.c2_layers.size(), "missing c2 layer");
            if (m_tree.c2_layers.size() == c2_idx)
                m_tree.c2_layers.emplace_back(Layer<Helios>{});

            auto &c2_inout = m_tree.c2_layers[c2_idx];

            const bool started_after_tip = (c2_inout.size() == c2_ext.start_idx);
            const bool started_at_tip    = (c2_inout.size() == (c2_ext.start_idx + 1));
            CHECK_AND_ASSERT_THROW_MES(started_after_tip || started_at_tip, "unexpected c2 layer start");

            // We updated the last hash
            if (started_at_tip)
            {
                CHECK_AND_ASSERT_THROW_MES(c2_ext.update_existing_last_hash, "expect to be updating last hash");
                c2_inout.back() = c2_ext.hashes.front();
            }
            else
            {
                CHECK_AND_ASSERT_THROW_MES(!c2_ext.update_existing_last_hash, "unexpected last hash update");
            }

            for (std::size_t i = started_at_tip ? 1 : 0; i < c2_ext.hashes.size(); ++i)
                c2_inout.emplace_back(c2_ext.hashes[i]);

            ++c2_idx;
        }

        parent_is_c1 = !parent_is_c1;
    }
}

CurveTreesV1::LastHashes CurveTreesGlobalTree::get_last_hashes() const
{
    CurveTreesV1::LastHashes last_hashes_out;
    auto &c1_last_hashes_out = last_hashes_out.c1_last_hashes;
    auto &c2_last_hashes_out = last_hashes_out.c2_last_hashes;

    const auto &c1_layers = m_tree.c1_layers;
    const auto &c2_layers = m_tree.c2_layers;

    // We started with c1 and then alternated, so c1 is the same size or 1 higher than c2
    CHECK_AND_ASSERT_THROW_MES(c1_layers.size() == c2_layers.size() || c1_layers.size() == (c2_layers.size() + 1),
        "unexpected number of curve layers");

    c1_last_hashes_out.reserve(c1_layers.size());
    c2_last_hashes_out.reserve(c2_layers.size());

    if (c1_layers.empty())
        return last_hashes_out;

    // Next parents will be c1
    bool parent_is_c1 = true;

    // Then get last chunks up until the root
    std::size_t c1_idx = 0;
    std::size_t c2_idx = 0;
    while (c1_last_hashes_out.size() < c1_layers.size() || c2_last_hashes_out.size() < c2_layers.size())
    {
        if (parent_is_c1)
        {
            CHECK_AND_ASSERT_THROW_MES(c1_layers.size() > c1_idx, "missing c1 layer");
            c1_last_hashes_out.push_back(c1_layers[c1_idx].back());
            ++c1_idx;
        }
        else
        {
            CHECK_AND_ASSERT_THROW_MES(c2_layers.size() > c2_idx, "missing c2 layer");
            c2_last_hashes_out.push_back(c2_layers[c2_idx].back());
            ++c2_idx;
        }

        parent_is_c1 = !parent_is_c1;
    }

    return last_hashes_out;
}

//----------------------------------------------------------------------------------------------------------------------
// Tenero's part
static std::string hex(const void *data, size_t n)
{
    static const char *digits = "0123456789abcdef";
    const unsigned char *p = static_cast<const unsigned char *>(data);
    std::string s;
    for (size_t i = 0; i < n; ++i) { s += digits[p[i] >> 4]; s += digits[p[i] & 15]; }
    return s;
}

static crypto::secret_key derive(const char *label, uint64_t i)
{
    std::string data(label);
    for (int b = 0; b < 8; ++b) data += static_cast<char>((i >> (8 * b)) & 0xff);
    crypto::hash h;
    crypto::cn_fast_hash(data.data(), data.size(), h);
    crypto::secret_key k;
    memcpy(k.data, h.data, 32);
    sc_reduce32(reinterpret_cast<unsigned char *>(k.data));
    return k;
}

static fcmp_pp::UnifiedOutput output(uint64_t i)
{
    crypto::public_key O, C;
    crypto::secret_key_to_public_key(derive("tenero curve tree vector O", i), O);
    crypto::secret_key_to_public_key(derive("tenero curve tree vector C", i), C);
    return fcmp_pp::UnifiedOutput{.unified_id = i, .output_pair = fcmp_pp::CarrotOutputPairV1{{O, (crypto::ec_point&)C}}};
}

int main(int argc, char **argv)
{
    if (argc != 2) { fprintf(stderr, "usage: tree_vectors <output.json>\n"); return 2; }
    const auto curve_trees = fcmp_pp::curve_trees::curve_trees_v1();
    CurveTreesGlobalTree tree(*curve_trees);
    // block sizes chosen to land on and just past every layer boundary: 38, 684 (38*18), 25992 (38*18*38)
    const std::vector<uint64_t> steps = {1, 1, 36, 1, 1, 100, 543, 1, 1, 1, 38, 2000, 22000, 1267, 1, 1, 1, 4, 500};
    std::string out = "{\n  \"schema\": 1,\n  \"name\": \"curve_tree_monero\",\n";
    out += "  \"description\": \"Roots of the FCMP++ curve tree as MONERO'S OWN code computes them (stressnet v0.19.0.0-beta.3.0): "
           "the production get_tree_extension grows the tree block by block, Monero's test audit re-hashes every layer after "
           "each block. Output i has key o_i G and commitment c_i G, o_i = Keccak256('tenero curve tree vector O' || i, 8 "
           "bytes LE) mod l, c_i likewise with 'C'; Carrot outputs (unbiased key-image generator). Made by "
           "reference/tools/carrot_harness/tree_vectors.cpp.\",\n";
    out += "  \"source\": {\"repo\": \"https://github.com/seraphis-migration/monero\", \"tag\": \"v0.19.0.0-beta.3.0\", \"licence\": \"BSD-3-Clause\"},\n";
    // the leaf tuples of the first outputs: the six Selene scalars {O.x, O.y, I.x, I.y, C.x, C.y}
    out += "  \"leaves\": [";
    for (uint64_t i = 0; i < 4; ++i)
    {
        const auto lt = curve_trees->leaf_tuple(output(i).output_pair);
        out += std::string(i ? ",\n    " : "\n    ") + "{\"index\": " + std::to_string(i) + ", \"scalars\": [";
        const Selene::Scalar *s = &lt.O_x;
        for (int j = 0; j < 6; ++j)
        {
            const crypto::ec_scalar b = curve_trees->m_c1->to_bytes(s[j]);
            out += std::string(j ? ", " : "") + "\"" + hex(&b, 32) + "\"";
        }
        out += "]}";
    }
    out += "],\n  \"blocks\": [";
    uint64_t n = 0;
    for (size_t si = 0; si < steps.size(); ++si)
    {
        std::vector<fcmp_pp::UnifiedOutput> outs;
        for (uint64_t k = 0; k < steps[si]; ++k) outs.push_back(output(n + k));
        const auto last_hashes = tree.get_last_hashes();
        const auto ext = curve_trees->get_tree_extension(n, last_hashes, {std::move(outs)});
        tree.extend_tree(ext);
        n += steps[si];
        if (!tree.audit_tree(n)) { fprintf(stderr, "audit failed at %llu\n", (unsigned long long)n); return 1; }
        const size_t layers = tree.m_tree.c1_layers.size() + tree.m_tree.c2_layers.size();
        if (layers != curve_trees->n_layers(n)) { fprintf(stderr, "layer count\n"); return 1; }
        crypto::ec_point root = (layers % 2 == 1)
            ? curve_trees->m_c1->to_bytes(tree.m_tree.c1_layers.back().back())
            : curve_trees->m_c2->to_bytes(tree.m_tree.c2_layers.back().back());
        out += std::string(si ? ",\n    " : "\n    ") + "{\"added\": " + std::to_string(steps[si]) + ", \"outputs\": " +
            std::to_string(n) + ", \"layers\": " + std::to_string(layers) + ", \"root\": \"" + hex(&root, 32) + "\"}";
    }
    out += "]\n}\n";
    FILE *f = fopen(argv[1], "wb");
    if (!f || fwrite(out.data(), 1, out.size(), f) != out.size() || fclose(f) != 0) { fprintf(stderr, "write\n"); return 1; }
    printf("wrote %s: %llu outputs\n", argv[1], (unsigned long long)n);
    return 0;
}
