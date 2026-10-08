// Tenero: makes tests/vectors/carrot_monero.json by running MONERO'S OWN carrot_core (stressnet v0.19.0.0-beta.3.0)
// on fixed pseudo-random inputs. It is built inside a Monero checkout by run.sh (in WSL) and never becomes part of
// Tenero's build: its only output is the JSON file, which crates/tenero-carrot/tests/upstream_monero_carrot.rs must
// reproduce bit for bit (docs/FCMP_CARROT_PLAN.md, G2). The inputs are recorded with the results, so the Rust test
// rebuilds every case from the file alone. The randomness is a seeded std::mt19937_64: reproducible, NOT secure, which
// is fine for test vectors and wrong for anything else.

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <map>
#include <optional>
#include <random>
#include <string>
#include <vector>

extern "C" {
#include "crypto/crypto-ops.h"
#include "mx25519.h"
}
#include "carrot_core/account_secrets.h"
#include "carrot_core/address_utils.h"
#include "carrot_core/destination.h"
#include "carrot_core/device_ram_borrowed.h"
#include "carrot_core/enote_utils.h"
#include "carrot_core/output_set_finalization.h"
#include "carrot_core/payment_proposal.h"
#include "carrot_core/scan.h"
#include "crypto/crypto.h"
#include "crypto/generators.h"
#include "ringct/rctOps.h"

using namespace carrot;

static std::mt19937_64 g_rng(0x7465'6e65'726f'4732ull); // "teneroG2"

static void rand_bytes(void *out, size_t n)
{
    unsigned char *p = static_cast<unsigned char *>(out);
    for (size_t i = 0; i < n; ++i)
        p[i] = static_cast<unsigned char>(g_rng() & 0xff);
}

static crypto::secret_key rand_scalar()
{
    unsigned char wide[64];
    rand_bytes(wide, 64);
    sc_reduce(wide);
    crypto::secret_key k;
    memcpy(k.data, wide, 32);
    return k;
}

static std::string hex(const void *data, size_t n)
{
    static const char *digits = "0123456789abcdef";
    const unsigned char *p = static_cast<const unsigned char *>(data);
    std::string s;
    for (size_t i = 0; i < n; ++i)
    {
        s += digits[p[i] >> 4];
        s += digits[p[i] & 15];
    }
    return s;
}

template <typename T>
static std::string h(const T &v) { return "\"" + hex(&v, sizeof(T)) + "\""; }
static std::string hk(const crypto::secret_key &k) { return "\"" + hex(k.data, 32) + "\""; }

[[noreturn]] static void fail(const char *what)
{
    fprintf(stderr, "harness check failed: %s\n", what);
    exit(1);
}

struct Account
{
    crypto::secret_key s_master, k_ps, s_vb, s_gp, k_gi, k_v, s_ga;
    crypto::public_key K_ps, K_s, K_v, K0_v;
    // the indices whose addresses the wallet knows: (0, 0) is the main address
    std::vector<std::pair<uint32_t, uint32_t>> indices;
    std::vector<CarrotDestinationV1> addresses;
    std::vector<crypto::secret_key> sub_scalars;
};

static crypto::public_key mul(const crypto::public_key &P, const crypto::secret_key &k)
{
    return rct::rct2pk(rct::scalarmultKey(rct::pk2rct(P), rct::sk2rct(k)));
}

static Account make_account()
{
    Account a;
    rand_bytes(a.s_master.data, 32); // a master secret is 32 random bytes, not a scalar
    make_carrot_provespend_key(a.s_master, a.k_ps);
    make_carrot_viewbalance_secret(a.s_master, a.s_vb);
    make_carrot_generateimage_preimage(a.s_vb, a.s_gp);
    make_carrot_partial_spend_pubkey(a.k_ps, a.K_ps);
    make_carrot_generateimage_key(a.s_gp, a.K_ps, a.k_gi);
    make_carrot_viewincoming_key(a.s_vb, a.k_v);
    make_carrot_generateaddress_secret(a.s_vb, a.s_ga);
    make_carrot_spend_pubkey(a.k_gi, a.k_ps, a.K_s);
    a.K_v = mul(a.K_s, a.k_v);
    a.K0_v = rct::rct2pk(rct::scalarmultBase(rct::sk2rct(a.k_v)));
    a.indices = {{0, 0}, {0, 1}, {0, 2}, {1, 0}, {2, 7}, {5, 16}, {0xffffffff, 0xffffffff}};
    for (const auto &[major, minor] : a.indices)
    {
        CarrotDestinationV1 d;
        crypto::secret_key s;
        if (major == 0 && minor == 0)
        {
            make_carrot_main_address_v1(a.K_s, a.K0_v, d);
            s = crypto::secret_key{};
            s.data[0] = 1;
        }
        else
        {
            make_carrot_subaddress_v1(a.K_s, a.K_v, generate_address_secret_ram_borrowed_device(a.s_ga), major, minor, d);
            crypto::secret_key p1, p2;
            make_carrot_address_index_preimage_1(a.s_ga, major, minor, p1);
            make_carrot_address_index_preimage_2(p1, major, minor, a.K_s, a.K_v, p2);
            make_carrot_subaddress_scalar(p2, a.K_s, s);
        }
        a.addresses.push_back(d);
        a.sub_scalars.push_back(s);
    }
    return a;
}

static std::string account_json(const Account &a)
{
    std::string s = "{\"s_master\": " + hk(a.s_master) + ", \"k_prove_spend\": " + hk(a.k_ps) +
        ", \"s_view_balance\": " + hk(a.s_vb) + ", \"s_generate_image_preimage\": " + hk(a.s_gp) +
        ", \"k_generate_image\": " + hk(a.k_gi) + ", \"k_view_incoming\": " + hk(a.k_v) +
        ", \"s_generate_address\": " + hk(a.s_ga) + ", \"partial_spend_pubkey\": " + h(a.K_ps) +
        ", \"spend_pubkey\": " + h(a.K_s) + ", \"view_pubkey\": " + h(a.K_v) + ", \"main_view_pubkey\": " + h(a.K0_v) +
        ", \"addresses\": [";
    for (size_t i = 0; i < a.indices.size(); ++i)
    {
        if (i) s += ", ";
        s += "{\"major\": " + std::to_string(a.indices[i].first) + ", \"minor\": " + std::to_string(a.indices[i].second) +
            ", \"spend_pubkey\": " + h(a.addresses[i].address_spend_pubkey) +
            ", \"view_pubkey\": " + h(a.addresses[i].address_view_pubkey) +
            ", \"subaddress_scalar\": " + hk(a.sub_scalars[i]) + "}";
    }
    return s + "]}";
}

static std::string dest_json(const CarrotDestinationV1 &d)
{
    return "{\"spend_pubkey\": " + h(d.address_spend_pubkey) + ", \"view_pubkey\": " + h(d.address_view_pubkey) +
        ", \"is_subaddress\": " + (d.is_subaddress ? "true" : "false") + ", \"payment_id\": " + h(d.payment_id) + "}";
}

static janus_anchor_t rand_anchor()
{
    janus_anchor_t a;
    do rand_bytes(a.bytes, 16); while (a == janus_anchor_t{});
    return a;
}

static std::string enote_json(const CarrotEnoteV1 &e)
{
    return "{\"onetime_address\": " + h(e.onetime_address) + ", \"amount_commitment\": " + h(e.amount_commitment) +
        ", \"amount_enc\": " + h(e.amount_enc) + ", \"view_tag\": " + h(e.view_tag) +
        ", \"ephemeral_pubkey\": " + h(e.enote_ephemeral_pubkey) + ", \"anchor_enc\": " + h(e.anchor_enc) +
        ", \"tx_first_key_image\": " + h(e.tx_first_key_image) + "}";
}

// What an account finds in an output, with the spend keys and the key image: x = k_gi s + k_g, y = k_ps s + k_t
static std::string found_json(const Account &a, size_t idx, const crypto::public_key &Ko, const std::string &kind,
    const crypto::secret_key &g, const crypto::secret_key &t, uint64_t amount, const crypto::secret_key &bf,
    CarrotEnoteType type, const payment_id_t &pid, const std::optional<janus_anchor_t> &msg)
{
    crypto::secret_key x, y;
    sc_muladd(to_bytes(x), to_bytes(a.k_gi), to_bytes(a.sub_scalars[idx]), to_bytes(g));
    sc_muladd(to_bytes(y), to_bytes(a.k_ps), to_bytes(a.sub_scalars[idx]), to_bytes(t));
    const rct::key opened = rct::addKeys(rct::scalarmultBase(rct::sk2rct(x)),
        rct::scalarmultKey(rct::pk2rct(crypto::get_T()), rct::sk2rct(y)));
    if (!(rct::rct2pk(opened) == Ko))
        fail("x G + y T is not the output key");
    crypto::ec_point I;
    crypto::derive_key_image_generator(Ko, /*biased=*/false, I);
    const rct::key ki = rct::scalarmultKey(rct::pt2rct(I), rct::sk2rct(x));
    return "{\"kind\": \"" + kind + "\", \"major\": " + std::to_string(a.indices[idx].first) +
        ", \"minor\": " + std::to_string(a.indices[idx].second) + ", \"amount\": " + std::to_string(amount) +
        ", \"blinding_factor\": " + hk(bf) + ", \"enote_type\": " + std::to_string(static_cast<int>(type)) +
        ", \"payment_id\": " + h(pid) + ", \"sender_extension_g\": " + hk(g) + ", \"sender_extension_t\": " + hk(t) +
        ", \"internal_message\": " + (msg ? h(*msg) : std::string("null")) + ", \"x\": " + hk(x) +
        ", \"y\": " + hk(y) + ", \"key_image\": " + h(ki) + "}";
}

static int index_of(const Account &a, const crypto::public_key &spend)
{
    for (size_t i = 0; i < a.addresses.size(); ++i)
        if (a.addresses[i].address_spend_pubkey == spend)
            return static_cast<int>(i);
    return -1;
}

// Scans one non-coinbase output with every account, as a view-all wallet does (internal, then external).
static std::string scan_json(const std::vector<Account> &accounts, const CarrotEnoteV1 &e, const encrypted_payment_id_t &pid_enc)
{
    std::string s = "[";
    bool first = true;
    for (size_t ai = 0; ai < accounts.size(); ++ai)
    {
        const Account &a = accounts[ai];
        crypto::secret_key g, t, bf;
        crypto::public_key spend;
        uint64_t amount;
        CarrotEnoteType type;
        janus_anchor_t msg;
        payment_id_t pid{};
        std::string found;
        if (try_scan_carrot_enote_internal_receiver(e, view_balance_secret_ram_borrowed_device(a.s_vb), g, t, spend,
                amount, bf, type, msg))
        {
            const int idx = index_of(a, spend);
            if (idx >= 0)
                found = found_json(a, idx, e.onetime_address, "internal", g, t, amount, bf, type, payment_id_t{}, msg);
        }
        if (found.empty())
        {
            mx25519_pubkey s_sr;
            try_make_carrot_shared_key_receiver(a.k_v, e.enote_ephemeral_pubkey, s_sr);
            if (try_scan_carrot_enote_external_receiver(e, pid_enc, s_sr, {&a.K_s, 1},
                    view_incoming_key_ram_borrowed_device(a.k_v), g, t, spend, amount, bf, pid, type))
            {
                const int idx = index_of(a, spend);
                if (idx >= 0)
                    found = found_json(a, idx, e.onetime_address, "external", g, t, amount, bf, type, pid, std::nullopt);
            }
        }
        if (!found.empty())
        {
            if (!first) s += ", ";
            first = false;
            s += "{\"account\": " + std::to_string(ai) + ", \"found\": " + found + "}";
        }
    }
    return s + "]";
}

struct SetCase
{
    std::string name;
    size_t sender;
    std::vector<CarrotPaymentProposalV1> normal;
    std::vector<CarrotPaymentProposalSelfSendV1> selfsend;
    std::optional<encrypted_payment_id_t> dummy_pid;
    bool view_balance; // internal self-sends (s_vb), else special (k_v)
};

static CarrotPaymentProposalSelfSendV1 selfsend(const CarrotDestinationV1 &d, uint64_t amount, CarrotEnoteType type,
    bool own_key, bool message)
{
    CarrotPaymentProposalSelfSendV1 p{};
    p.destination_address_spend_pubkey = d.address_spend_pubkey;
    p.is_subaddress = d.is_subaddress;
    p.amount = amount;
    p.enote_type = type;
    if (own_key)
        p.enote_ephemeral_privkey = rand_scalar();
    if (message)
        p.internal_message = rand_anchor();
    return p;
}

static encrypted_payment_id_t rand_pid_enc()
{
    encrypted_payment_id_t p;
    rand_bytes(p.bytes, 8);
    return p;
}

int main(int argc, char **argv)
{
    if (argc != 2)
    {
        fprintf(stderr, "usage: carrot_vectors <output.json>\n");
        return 2;
    }
    std::vector<Account> accounts;
    for (int i = 0; i < 6; ++i)
        accounts.push_back(make_account());
    const auto addr = [&](size_t a, size_t i) { return accounts[a].addresses[i]; };
    const auto integrated = [&](size_t a) {
        CarrotDestinationV1 d;
        payment_id_t pid;
        do rand_bytes(pid.bytes, 8); while (pid == null_payment_id);
        make_carrot_integrated_address_v1(accounts[a].K_s, accounts[a].K0_v, pid, d);
        return d;
    };
    const auto pay = [&](const CarrotDestinationV1 &d, uint64_t amount) {
        return CarrotPaymentProposalV1{d, amount, rand_anchor()};
    };

    std::vector<SetCase> cases;
    for (bool vb : {true, false})
    {
        const std::string mode = vb ? " (internal self-sends)" : " (special self-sends)";
        cases.push_back({"2 outputs: a main address and change" + mode, 0,
            {pay(addr(1, 0), 7000000)}, {selfsend(addr(0, 0), 3000, CarrotEnoteType::CHANGE, false, false)},
            rand_pid_enc(), vb});
        cases.push_back({"2 outputs: a subaddress and change to a subaddress" + mode, 0,
            {pay(addr(2, 4), 123456789)}, {selfsend(addr(0, 1), 1, CarrotEnoteType::CHANGE, false, false)},
            rand_pid_enc(), vb});
        cases.push_back({"2 outputs: an integrated address and change" + mode, 1,
            {pay(integrated(3), 5)}, {selfsend(addr(1, 0), 99, CarrotEnoteType::CHANGE, false, false)},
            std::nullopt, vb});
        cases.push_back({"2 outputs: two self-sends, payment and change" + mode, 2,
            {}, {selfsend(addr(2, 3), 100, CarrotEnoteType::CHANGE, true, vb),
                 selfsend(addr(2, 0), 200, CarrotEnoteType::PAYMENT, false, false)},
            rand_pid_enc(), vb});
        cases.push_back({"4 outputs: main, subaddress, integrated, and change" + mode, 3,
            {pay(addr(4, 0), 1), pay(addr(5, 6), 18446744073709551615ull), pay(integrated(0), 0)},
            {selfsend(addr(3, 5), 77, CarrotEnoteType::CHANGE, true, vb)},
            std::nullopt, vb});
        std::vector<CarrotPaymentProposalV1> many;
        for (int i = 0; i < 15; ++i)
            many.push_back(pay(addr(1 + (i % 5), i % 7), 1000 + i));
        cases.push_back({"16 outputs: fifteen payments and change" + mode, 0, many,
            {selfsend(addr(0, 2), 5, CarrotEnoteType::CHANGE, true, false)}, rand_pid_enc(), vb});
    }

    std::string out = "{\n  \"schema\": 1,\n  \"name\": \"carrot_monero\",\n";
    out += "  \"description\": \"Results of MONERO'S OWN carrot_core (stressnet v0.19.0.0-beta.3.0) on fixed pseudo-random "
           "inputs, made by reference/tools/carrot_harness (run in WSL). Accounts with their secrets, public keys and "
           "addresses; output sets (the proposals given, the outputs made, the encrypted payment ID, the order); "
           "coinbase outputs; and what each account finds in every output (spend keys x, y and key image included). "
           "Accounts not listed in an output's 'found' must find nothing. Plus unclamped X25519 products and the "
           "unbiased hash to point.\",\n";
    out += "  \"source\": {\"repo\": \"https://github.com/seraphis-migration/monero\", \"tag\": \"v0.19.0.0-beta.3.0\", "
           "\"licence\": \"BSD-3-Clause\"},\n";
    out += "  \"accounts\": [\n";
    for (size_t i = 0; i < accounts.size(); ++i)
        out += std::string(i ? ",\n" : "") + "    " + account_json(accounts[i]);
    out += "\n  ],\n  \"output_sets\": [\n";
    for (size_t ci = 0; ci < cases.size(); ++ci)
    {
        const SetCase &c = cases[ci];
        const crypto::key_image first_ki = rct::rct2ki(rct::scalarmultBase(rct::sk2rct(rand_scalar())));
        const Account &sender = accounts[c.sender];
        view_balance_secret_ram_borrowed_device vb_dev(sender.s_vb);
        view_incoming_key_ram_borrowed_device kv_dev(sender.k_v);
        std::vector<RCTOutputEnoteProposal> outs;
        encrypted_payment_id_t pid_enc;
        std::vector<std::pair<bool, size_t>> order;
        get_output_enote_proposals(c.normal, c.selfsend, c.dummy_pid, c.view_balance ? &vb_dev : nullptr,
            c.view_balance ? nullptr : &kv_dev, first_ki, outs, pid_enc, &order);
        std::string s = "    {\"name\": \"" + c.name + "\", \"sender\": " + std::to_string(c.sender) +
            ", \"view_balance\": " + (c.view_balance ? "true" : "false") + ", \"first_key_image\": " + h(first_ki) +
            ", \"dummy_encrypted_payment_id\": " + (c.dummy_pid ? h(*c.dummy_pid) : std::string("null")) +
            ",\n     \"normal\": [";
        for (size_t i = 0; i < c.normal.size(); ++i)
            s += std::string(i ? ", " : "") + "{\"destination\": " + dest_json(c.normal[i].destination) +
                ", \"amount\": " + std::to_string(c.normal[i].amount) + ", \"randomness\": " + h(c.normal[i].randomness) + "}";
        s += "],\n     \"selfsend\": [";
        for (size_t i = 0; i < c.selfsend.size(); ++i)
        {
            const auto &p = c.selfsend[i];
            s += std::string(i ? ", " : "") + "{\"destination_spend_pubkey\": " + h(p.destination_address_spend_pubkey) +
                ", \"is_subaddress\": " + (p.is_subaddress ? "true" : "false") + ", \"amount\": " + std::to_string(p.amount) +
                ", \"enote_type\": " + std::to_string(static_cast<int>(p.enote_type)) +
                ", \"ephemeral_privkey\": " + (p.enote_ephemeral_privkey ? hk(*p.enote_ephemeral_privkey) : std::string("null")) +
                ", \"internal_message\": " + (p.internal_message ? h(*p.internal_message) : std::string("null")) + "}";
        }
        s += "],\n     \"encrypted_payment_id\": " + h(pid_enc) + ", \"order\": [";
        for (size_t i = 0; i < order.size(); ++i)
            s += std::string(i ? ", " : "") + "[" + (order[i].first ? "true" : "false") + ", " + std::to_string(order[i].second) + "]";
        s += "],\n     \"outputs\": [";
        for (size_t i = 0; i < outs.size(); ++i)
            s += std::string(i ? ",\n       " : "\n       ") + "{\"enote\": " + enote_json(outs[i].enote) +
                ", \"amount\": " + std::to_string(outs[i].amount) + ", \"blinding_factor\": " + hk(outs[i].amount_blinding_factor) +
                ", \"found\": " + scan_json(accounts, outs[i].enote, pid_enc) + "}";
        out += s + "]}" + (ci + 1 < cases.size() ? ",\n" : "\n");
    }

    // coinbase outputs: three miners, block 7777
    {
        std::vector<CarrotPaymentProposalV1> miners = {pay(addr(0, 0), 2000000000), pay(addr(3, 0), 1), pay(addr(5, 0), 0)};
        std::vector<CarrotCoinbaseEnoteV1> enotes;
        get_coinbase_output_enotes(miners, 7777, enotes);
        out += "  ],\n  \"coinbase\": {\"block_index\": 7777, \"normal\": [";
        for (size_t i = 0; i < miners.size(); ++i)
            out += std::string(i ? ", " : "") + "{\"destination\": " + dest_json(miners[i].destination) +
                ", \"amount\": " + std::to_string(miners[i].amount) + ", \"randomness\": " + h(miners[i].randomness) + "}";
        out += "],\n    \"outputs\": [";
        for (size_t i = 0; i < enotes.size(); ++i)
        {
            const auto &e = enotes[i];
            std::string found = "[";
            bool first = true;
            for (size_t ai = 0; ai < accounts.size(); ++ai)
            {
                const Account &a = accounts[ai];
                mx25519_pubkey s_sr;
                try_make_carrot_shared_key_receiver(a.k_v, e.enote_ephemeral_pubkey, s_sr);
                crypto::secret_key g, t;
                if (try_scan_carrot_coinbase_enote_receiver(e, s_sr, a.K_s, a.K0_v, g, t))
                {
                    crypto::secret_key one{};
                    one.data[0] = 1;
                    found += std::string(first ? "" : ", ") + "{\"account\": " + std::to_string(ai) + ", \"found\": " +
                        found_json(a, 0, e.onetime_address, "coinbase", g, t, e.amount, one, CarrotEnoteType::PAYMENT,
                            payment_id_t{}, std::nullopt) + "}";
                    first = false;
                }
            }
            out += std::string(i ? ",\n      " : "\n      ") + "{\"onetime_address\": " + h(e.onetime_address) +
                ", \"amount\": " + std::to_string(e.amount) + ", \"view_tag\": " + h(e.view_tag) +
                ", \"ephemeral_pubkey\": " + h(e.enote_ephemeral_pubkey) + ", \"anchor_enc\": " + h(e.anchor_enc) +
                ", \"block_index\": " + std::to_string(e.block_index) + ", \"found\": " + found + "]}";
        }
        out += "]},\n";
    }

    // unclamped X25519: random scalars times random u (some with the top bit set, some not on the curve), and edge u
    {
        static const mx25519_impl *impl = mx25519_select_impl(MX25519_TYPE_AUTO);
        out += "  \"x25519\": [";
        for (int i = 0; i < 20; ++i)
        {
            crypto::secret_key k = rand_scalar();
            mx25519_pubkey u, r;
            rand_bytes(u.data, 32);
            if (i == 0) memset(u.data, 0, 32);
            if (i == 1) { memset(u.data, 0, 32); u.data[0] = 1; }
            if (i == 2) { memset(u.data, 0xff, 32); u.data[0] = 0xec; u.data[31] = 0x7f; } // p - 1
            if (i % 2 == 1) u.data[31] |= 0x80;
            mx25519_scmul_key_unclamped(impl, &r, reinterpret_cast<const mx25519_privkey *>(&k), &u);
            out += std::string(i ? ",\n    " : "\n    ") + "{\"scalar\": " + hk(k) + ", \"u\": " + h(u) + ", \"product\": " + h(r) + "}";
        }
        out += "],\n";
    }

    // the unbiased hash to point (the key-image generator of a Carrot output)
    {
        out += "  \"hash_to_point\": [";
        for (int i = 0; i < 10; ++i)
        {
            crypto::public_key p;
            rand_bytes(&p, 32);
            crypto::ec_point I;
            crypto::derive_key_image_generator(p, false, I);
            out += std::string(i ? ", " : "") + "{\"input\": " + h(p) + ", \"point\": " + h(I) + "}";
        }
        out += "]\n}\n";
    }

    FILE *f = fopen(argv[1], "wb");
    if (!f || fwrite(out.data(), 1, out.size(), f) != out.size() || fclose(f) != 0)
        fail("cannot write the output file");
    printf("wrote %s (%zu bytes)\n", argv[1], out.size());
    return 0;
}
