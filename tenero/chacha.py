"""ChaCha20 (RFC 8439) in numpy, vectorised over many blocks at once.

This is the cryptographic primitive the proof of work is built on. Two uses:

  chacha20_blocks(...)  the standard block function: a 256-bit key, a 32-bit block counter and
                        a 96-bit nonce give 64 bytes of keystream. It is checked against OpenSSL's
                        implementation in the tests (and against a fixed known-answer vector).
  chacha_core(state)    the same permutation (20 rounds, then the feed-forward add) applied to an
                        arbitrary 16-word state. The proof of work uses it as a mixing function.

All arithmetic is on unsigned 32-bit words (they wrap around, exactly as on a GPU). Words are
little-endian, as in the RFC.
"""
import numpy as np

# "expand 32-byte k"
CONSTANTS = np.array([0x61707865, 0x3320646E, 0x79622D32, 0x6B206574], dtype=np.uint32)

_U16, _U12, _U8, _U7 = (np.uint32(n) for n in (16, 12, 8, 7))
_U32 = np.uint32(32)


def _rotl(x, n):
    return (x << n) | (x >> (_U32 - n))


def _quarter_round(s, a, b, c, d):
    s[a] += s[b]
    s[d] ^= s[a]
    s[d] = _rotl(s[d], _U16)
    s[c] += s[d]
    s[b] ^= s[c]
    s[b] = _rotl(s[b], _U12)
    s[a] += s[b]
    s[d] ^= s[a]
    s[d] = _rotl(s[d], _U8)
    s[c] += s[d]
    s[b] ^= s[c]
    s[b] = _rotl(s[b], _U7)


def chacha_core(state, rounds=20):
    """The ChaCha permutation plus feed-forward on an (n, 16) uint32 array. Returns (n, 16)."""
    state = np.ascontiguousarray(state, dtype=np.uint32)
    original = [state[:, i].copy() for i in range(16)]
    s = [w.copy() for w in original]
    for _ in range(rounds // 2):
        _quarter_round(s, 0, 4, 8, 12)      # column round
        _quarter_round(s, 1, 5, 9, 13)
        _quarter_round(s, 2, 6, 10, 14)
        _quarter_round(s, 3, 7, 11, 15)
        _quarter_round(s, 0, 5, 10, 15)     # diagonal round
        _quarter_round(s, 1, 6, 11, 12)
        _quarter_round(s, 2, 7, 8, 13)
        _quarter_round(s, 3, 4, 9, 14)
    for i in range(16):
        s[i] += original[i]
    return np.stack(s, axis=1)


def chacha20_blocks(keys, counters, nonces):
    """Standard ChaCha20 blocks. keys: (n, 8) uint32; counters: (n,) uint32; nonces: (n, 3)
    uint32. Returns (n, 16) uint32: word i of block j is keystream bytes 4i..4i+3 of that block."""
    keys = np.asarray(keys, dtype=np.uint32)
    n = len(keys)
    state = np.empty((n, 16), dtype=np.uint32)
    state[:, 0:4] = CONSTANTS
    state[:, 4:12] = keys
    state[:, 12] = np.asarray(counters, dtype=np.uint32)
    state[:, 13:16] = np.asarray(nonces, dtype=np.uint32)
    return chacha_core(state)


def key_words(key_bytes):
    """A 32-byte key as 8 little-endian uint32 words."""
    if len(key_bytes) != 32:
        raise ValueError("a ChaCha20 key is 32 bytes")
    return np.frombuffer(key_bytes, dtype="<u4").astype(np.uint32)


def keystream(key_bytes, nblocks, start_counter=0, nonce=(0, 0, 0)):
    """`nblocks` blocks of keystream as an (nblocks, 16) uint32 array (64 bytes each)."""
    keys = np.tile(key_words(key_bytes), (nblocks, 1))
    counters = (np.arange(nblocks, dtype=np.uint64) + start_counter).astype(np.uint32)
    nonces = np.tile(np.asarray(nonce, dtype=np.uint32), (nblocks, 1))
    return chacha20_blocks(keys, counters, nonces)
