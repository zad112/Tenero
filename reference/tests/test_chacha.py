import numpy as np
import pytest

from tenero import chacha

# RFC 8439 section 2.3.2: key 00 01 .. 1f, counter 1, nonce 00 00 00 09 00 00 00 4a 00 00 00 00.
# The expected block is the one printed in the RFC (also confirmed against OpenSSL).
RFC_KEY = bytes(range(32))
RFC_NONCE = bytes([0, 0, 0, 9, 0, 0, 0, 0x4A, 0, 0, 0, 0])
RFC_BLOCK = bytes.fromhex(
    "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e"
    "d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e")


def test_the_rfc_8439_test_block():
    words = chacha.chacha20_blocks(chacha.key_words(RFC_KEY)[None], np.array([1]),
                                   np.frombuffer(RFC_NONCE, dtype="<u4")[None])
    assert words[0].astype("<u4").tobytes() == RFC_BLOCK


def test_keystream_shapes_and_counters():
    ks = chacha.keystream(RFC_KEY, 5, start_counter=3, nonce=(1, 2, 3))
    assert ks.shape == (5, 16) and ks.dtype == np.uint32
    one = chacha.keystream(RFC_KEY, 1, start_counter=5, nonce=(1, 2, 3))
    assert np.array_equal(ks[2], one[0])                 # block 5 is the third of 3..7


def test_the_counter_wraps_like_a_32_bit_counter():
    ks = chacha.keystream(RFC_KEY, 3, start_counter=2**32 - 1)
    zero = chacha.keystream(RFC_KEY, 1, start_counter=0)
    assert np.array_equal(ks[1], zero[0])


def test_key_validation():
    with pytest.raises(ValueError):
        chacha.key_words(b"short")


def test_different_keys_counters_and_nonces_give_different_blocks():
    a = chacha.keystream(RFC_KEY, 1)
    assert not np.array_equal(a, chacha.keystream(bytes(reversed(RFC_KEY)), 1))
    assert not np.array_equal(a, chacha.keystream(RFC_KEY, 1, start_counter=1))
    assert not np.array_equal(a, chacha.keystream(RFC_KEY, 1, nonce=(0, 0, 1)))


def py_core(x, rounds=20):
    """An independent scalar ChaCha permutation + feed-forward on any 16 words (Python ints)."""
    m = 0xFFFFFFFF

    def rotl(v, n):
        return ((v << n) | (v >> (32 - n))) & m

    def qr(s, a, b, c, d):
        s[a] = (s[a] + s[b]) & m; s[d] = rotl(s[d] ^ s[a], 16)
        s[c] = (s[c] + s[d]) & m; s[b] = rotl(s[b] ^ s[c], 12)
        s[a] = (s[a] + s[b]) & m; s[d] = rotl(s[d] ^ s[a], 8)
        s[c] = (s[c] + s[d]) & m; s[b] = rotl(s[b] ^ s[c], 7)

    s = [int(v) for v in x]
    for _ in range(rounds // 2):
        qr(s, 0, 4, 8, 12); qr(s, 1, 5, 9, 13); qr(s, 2, 6, 10, 14); qr(s, 3, 7, 11, 15)
        qr(s, 0, 5, 10, 15); qr(s, 1, 6, 11, 12); qr(s, 2, 7, 8, 13); qr(s, 3, 4, 9, 14)
    return [(s[i] + int(x[i])) & m for i in range(16)]


def test_the_vectorised_core_matches_an_independent_scalar_one_on_arbitrary_states():
    rng = np.random.default_rng(3)
    states = rng.integers(0, 2**32, size=(40, 16), dtype=np.uint64).astype(np.uint32)
    states[0] = 0
    states[1] = 0xFFFFFFFF
    got = chacha.chacha_core(states)
    for row, out in zip(states, got):
        assert [int(v) for v in out] == py_core(row)


def test_the_scalar_core_reproduces_the_rfc_block():
    state = list(chacha.CONSTANTS) + list(chacha.key_words(RFC_KEY)) + [1] + \
        list(np.frombuffer(RFC_NONCE, dtype="<u4"))
    out = py_core(state)
    assert np.array(out, dtype="<u4").tobytes() == RFC_BLOCK


def test_fewer_rounds_give_a_different_result():
    x = np.arange(16, dtype=np.uint32)[None]
    assert not np.array_equal(chacha.chacha_core(x, 8), chacha.chacha_core(x, 20))


def test_it_agrees_with_openssl_on_random_inputs():
    ciphers = pytest.importorskip("cryptography.hazmat.primitives.ciphers")
    rng = np.random.default_rng(0)
    for _ in range(25):
        key, nonce = rng.bytes(32), rng.bytes(12)
        counter, nblocks = int(rng.integers(0, 2**32 - 300)), int(rng.integers(1, 120))
        enc = ciphers.Cipher(ciphers.algorithms.ChaCha20(key, counter.to_bytes(4, "little") + nonce),
                             mode=None).encryptor()
        expected = enc.update(b"\x00" * (64 * nblocks))
        got = chacha.keystream(key, nblocks, counter,
                               tuple(np.frombuffer(nonce, dtype="<u4"))).astype("<u4").tobytes()
        assert got == expected
