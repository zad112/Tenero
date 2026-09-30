//! The Noise channel: a real handshake over real loopback sockets, and then what an attacker on the wire can and
//! cannot do to it.

use std::io::{self, Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use tenero_net::noise::{
    handshake_initiator, handshake_responder, prologue, NodeKey, NoiseError, Secured, MAX_CHUNK,
};
use tenero_net::{encode, FrameDecoder, Message};

const CHAIN: [u8; 32] = [7; 32];

fn tcp_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = TcpStream::connect(addr).unwrap();
    let (server, _) = listener.accept().unwrap();
    (client, server)
}

/// A finished handshake between two fresh keys: `(initiator side, responder side, initiator key, responder key)`.
fn secured_pair() -> (Secured, Secured, NodeKey, NodeKey) {
    let (ka, kb) = (NodeKey::generate(), NodeKey::generate());
    let (mut c, mut s) = tcp_pair();
    let (kb2, ka2) = (kb.clone(), ka.clone());
    let p = prologue(1, &CHAIN);
    let p2 = p.clone();
    let t = thread::spawn(move || handshake_responder(&mut s, &kb2, &p2).unwrap());
    let a = handshake_initiator(&mut c, &ka2, &p).unwrap();
    let b = t.join().unwrap();
    (a, b, ka, kb)
}

/// Bytes in, bytes out: what a handshake sees when the other end is a script.
struct Duplex {
    input: Cursor<Vec<u8>>,
    output: Vec<u8>,
}

impl Duplex {
    fn new(input: Vec<u8>) -> Duplex {
        Duplex {
            input: Cursor::new(input),
            output: Vec::new(),
        }
    }
}

impl Read for Duplex {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.input.read(buf)
    }
}

impl Write for Duplex {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.output.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

// ---- the handshake ---------------------------------------------------------------------------------------

#[test]
fn two_nodes_shake_hands_each_learns_the_others_key_and_they_talk_both_ways() {
    // the initiator's key is rebuilt from its saved private half, as a node that loaded it from disk would
    let ka = NodeKey::from_bytes(&NodeKey::generate().to_bytes()).unwrap();
    let kb = NodeKey::generate();
    let (mut c, mut s) = tcp_pair();
    let (kb2, p) = (kb.clone(), prologue(1, &CHAIN));
    let p2 = p.clone();
    let t = thread::spawn(move || handshake_responder(&mut s, &kb2, &p2).map(|b| (b, s)));
    let mut a = handshake_initiator(&mut c, &ka, &p).unwrap();
    let (mut b, mut s) = t.join().unwrap().unwrap();
    assert_eq!(a.remote_public, kb.public());
    assert_eq!(
        b.remote_public,
        ka.public(),
        "the public key derived from a saved private key is the real one"
    );
    assert_ne!(ka.public(), kb.public());

    // initiator to responder, and back
    let secret = b"a message nobody on the wire should read";
    let wire = a.writer.seal(secret).unwrap();
    assert!(
        !wire.windows(secret.len()).any(|w| w == secret),
        "the plaintext is on the wire"
    );
    c.write_all(&wire).unwrap();
    assert_eq!(b.reader.read_chunk(&mut s).unwrap(), secret);
    b.writer.write_all(&mut s, b"and back").unwrap();
    assert_eq!(a.reader.read_chunk(&mut c).unwrap(), b"and back");
    // the counters move independently: a second each way
    a.writer.write_all(&mut c, b"two").unwrap();
    assert_eq!(b.reader.read_chunk(&mut s).unwrap(), b"two");
}

#[test]
fn another_chain_or_protocol_version_fails_the_handshake_on_both_sides() {
    for (pa, pb) in [
        (prologue(1, &CHAIN), prologue(1, &[8; 32])),
        (prologue(1, &CHAIN), prologue(2, &CHAIN)),
    ] {
        let (mut c, mut s) = tcp_pair();
        let (ka, kb) = (NodeKey::generate(), NodeKey::generate());
        let t = thread::spawn(move || handshake_responder(&mut s, &kb, &pb).is_err());
        let a = handshake_initiator(&mut c, &ka, &pa);
        drop(c);
        assert!(a.is_err(), "the dialling side was let through");
        assert!(t.join().unwrap(), "the accepting side was let through");
    }
    assert_ne!(prologue(1, &CHAIN), prologue(2, &CHAIN));
    assert_ne!(prologue(1, &CHAIN), prologue(1, &[8; 32]));
}

#[test]
fn a_handshake_fed_garbage_fails_and_never_panics() {
    let key = NodeKey::generate();
    let p = prologue(1, &CHAIN);
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for i in 0..3000 {
        let n = (rng.next() % 300) as usize;
        let junk = rng.bytes(n);
        assert!(
            handshake_responder(&mut Duplex::new(junk.clone()), &key, &p).is_err(),
            "case {i}"
        );
        assert!(
            handshake_initiator(&mut Duplex::new(junk), &key, &p).is_err(),
            "case {i}"
        );
    }
    // specific shapes: a zero length, a length over the limit, a message cut short, a length and no bytes
    for bytes in [
        vec![0, 0],
        vec![2, 1],
        vec![0xff, 0xff],
        vec![0, 32, 1, 2, 3],
        vec![0, 32],
        vec![],
        vec![0],
    ] {
        assert!(handshake_responder(&mut Duplex::new(bytes.clone()), &key, &p).is_err());
        assert!(handshake_initiator(&mut Duplex::new(bytes), &key, &p).is_err());
    }
    // a well-formed first message followed by rubbish in place of the third
    let mut good_first = Duplex::new(vec![]);
    let _ = handshake_initiator(&mut good_first, &key, &p); // writes message 1, then fails reading message 2
    let mut script = good_first.output.clone();
    script.extend_from_slice(&[0, 100]);
    script.extend_from_slice(&[9u8; 100]);
    assert!(handshake_responder(&mut Duplex::new(script), &key, &p).is_err());
}

// ---- the channel -----------------------------------------------------------------------------------------

#[test]
fn big_payloads_are_cut_into_chunks_and_come_back_whole() {
    let (mut a, mut b, ..) = secured_pair();
    assert_eq!(
        MAX_CHUNK, 65_519,
        "a Noise message is at most 65,535 bytes, 16 of them the tag"
    );
    let mut rng = Rng(99);
    for n in [
        1usize,
        100,
        MAX_CHUNK - 1,
        MAX_CHUNK,
        MAX_CHUNK + 1,
        3 * MAX_CHUNK,
        200_000,
        16 * 1024 * 1024 + 10,
    ] {
        let data = rng.bytes(n);
        let wire = a.writer.seal(&data).unwrap();
        let chunks = n.div_ceil(MAX_CHUNK);
        assert_eq!(wire.len(), n + chunks * (2 + 16), "{n} bytes");
        let mut stream = Cursor::new(wire);
        let mut got = Vec::new();
        let mut count = 0;
        while (stream.position() as usize) < stream.get_ref().len() {
            let chunk = b.reader.read_chunk(&mut stream).unwrap();
            assert!(chunk.len() <= MAX_CHUNK && !chunk.is_empty());
            got.extend_from_slice(&chunk);
            count += 1;
        }
        assert_eq!(count, chunks);
        assert!(got == data, "{n} bytes came back changed");
    }
    // nothing is nothing
    assert!(a.writer.seal(&[]).unwrap().is_empty());
}

#[test]
fn protocol_messages_go_through_the_channel_and_the_frame_decoder() {
    let (mut a, mut b, ..) = secured_pair();
    let messages = vec![
        Message::Ping(7),
        Message::GetAddrs,
        Message::NewBlock {
            id: [1; 32],
            height: 9,
            cumulative_work: [2; 32],
        },
        Message::GetBlockIds {
            locator: vec![[3; 32]; 32],
        },
    ];
    let mut wire = Vec::new();
    for m in &messages {
        wire.extend(a.writer.seal(&encode(m).unwrap()).unwrap());
    }
    let mut stream = Cursor::new(wire);
    let mut decoder = FrameDecoder::new();
    let mut got = Vec::new();
    while (stream.position() as usize) < stream.get_ref().len() {
        decoder.push(&b.reader.read_chunk(&mut stream).unwrap());
        while let Some(m) = decoder.next_message().unwrap() {
            got.push(m);
        }
    }
    assert_eq!(got, messages);
}

#[test]
fn a_damaged_chunk_is_refused_wherever_the_damage_is() {
    let (mut a, _b, ..) = secured_pair();
    let wire = a.writer.seal(b"some payload of modest length").unwrap();
    let ct = &wire[2..];
    for i in 0..ct.len() {
        for bit in [0u8, 7] {
            // a fresh receiver each time: the counter must be at zero for the chunk to be the next expected one
            let (mut a2, mut b2, ..) = secured_pair();
            let mut sealed = a2.writer.seal(b"some payload of modest length").unwrap();
            sealed[2 + i] ^= 1 << bit;
            assert!(
                matches!(b2.reader.open(&sealed[2..]), Err(NoiseError::Decrypt)),
                "byte {i}, bit {bit}"
            );
        }
    }
}

#[test]
fn replayed_dropped_and_reordered_chunks_are_refused() {
    let chunk = |a: &mut Secured, text: &[u8]| a.writer.seal(text).unwrap()[2..].to_vec();
    // replay
    let (mut a, mut b, ..) = secured_pair();
    let c1 = chunk(&mut a, b"one");
    assert_eq!(b.reader.open(&c1).unwrap(), b"one");
    assert!(
        matches!(b.reader.open(&c1), Err(NoiseError::Decrypt)),
        "a replay"
    );
    // a chunk dropped from the middle
    let (mut a, mut b, ..) = secured_pair();
    let _lost = chunk(&mut a, b"one");
    let c2 = chunk(&mut a, b"two");
    assert!(
        matches!(b.reader.open(&c2), Err(NoiseError::Decrypt)),
        "after a drop"
    );
    // two swapped
    let (mut a, mut b, ..) = secured_pair();
    let c1 = chunk(&mut a, b"one");
    let c2 = chunk(&mut a, b"two");
    assert!(
        matches!(b.reader.open(&c2), Err(NoiseError::Decrypt)),
        "reordered"
    );
    let _ = c1;
    // a chunk sent one way is not valid the other way round (the two directions have their own keys)
    let (mut a, mut b, ..) = secured_pair();
    let c = chunk(&mut a, b"mine");
    assert!(
        matches!(a.reader.open(&c), Err(NoiseError::Decrypt)),
        "reflected"
    );
    let _ = &mut b;
}

#[test]
fn chunks_with_a_forbidden_length_or_cut_short_are_refused() {
    let (_a, mut b, ..) = secured_pair();
    for (bytes, name) in [
        (vec![0u8, 0], "length zero"),
        (vec![0, 1, 9], "one byte"),
        (vec![0, 16], "exactly a tag (nothing inside)"),
    ] {
        let mut s = Cursor::new(bytes);
        assert!(
            matches!(b.reader.read_chunk(&mut s), Err(NoiseError::BadLength(_))),
            "{name}"
        );
    }
    let mut cut = Cursor::new(vec![0u8, 40, 1, 2, 3]);
    assert!(
        matches!(b.reader.read_chunk(&mut cut), Err(NoiseError::Io(_))),
        "cut short"
    );
    let mut none = Cursor::new(vec![0u8]);
    assert!(matches!(
        b.reader.read_chunk(&mut none),
        Err(NoiseError::Io(_))
    ));
    assert!(matches!(
        b.reader.open(&[1, 2, 3]),
        Err(NoiseError::BadLength(3))
    ));
    assert!(
        matches!(b.reader.open(&[0; 16]), Err(NoiseError::BadLength(16))),
        "a tag and nothing inside"
    );
}

#[test]
fn a_nonce_is_never_reused() {
    let (mut a, mut b, ..) = secured_pair();
    a.writer.set_nonce_for_tests(u64::MAX - 1);
    b.reader.set_nonce_for_tests(u64::MAX - 1);
    let last = a.writer.seal(b"the last one").unwrap();
    assert_eq!(b.reader.open(&last[2..]).unwrap(), b"the last one");
    assert!(matches!(
        a.writer.seal(b"one too many"),
        Err(NoiseError::NonceExhausted)
    ));
    assert!(matches!(
        b.reader.open(&last[2..]),
        Err(NoiseError::NonceExhausted)
    ));
    // and a multi-chunk seal stops at the limit rather than wrapping
    let (mut a, _b, ..) = secured_pair();
    a.writer.set_nonce_for_tests(u64::MAX - 1);
    assert!(matches!(
        a.writer.seal(&vec![0u8; 2 * MAX_CHUNK]),
        Err(NoiseError::NonceExhausted)
    ));
}

// ---- keys ------------------------------------------------------------------------------------------------

#[test]
fn keys_are_random_survive_saving_and_never_print_the_private_half() {
    let a = NodeKey::generate();
    let b = NodeKey::generate();
    assert_ne!(a.public(), b.public());
    assert_ne!(a.to_bytes(), b.to_bytes());
    let again = NodeKey::from_bytes(&a.to_bytes()).unwrap();
    assert_eq!(again.public(), a.public());
    assert!(NodeKey::from_bytes(&[0; 31]).is_err());
    assert!(NodeKey::from_bytes(&[0; 33]).is_err());
    let shown = format!("{a:?}");
    let private_hex: String = a.to_bytes().iter().map(|x| format!("{x:02x}")).collect();
    assert!(
        !shown.contains(&private_hex),
        "the private key is in the debug output"
    );
    let public_hex: String = a.public().iter().map(|x| format!("{x:02x}")).collect();
    assert!(shown.contains(&public_hex));

    // on disk: made on first use, the same afterwards, and a damaged file is refused rather than replaced
    let path = std::env::temp_dir().join(format!("tenero-nodekey-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let first = NodeKey::load_or_create(&path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap().len(), 32);
    let second = NodeKey::load_or_create(&path).unwrap();
    assert_eq!(first.public(), second.public());
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    assert!(!std::path::PathBuf::from(tmp).exists());
    std::fs::write(&path, [1u8; 31]).unwrap();
    assert!(NodeKey::load_or_create(&path).is_err());
    assert_eq!(
        std::fs::read(&path).unwrap().len(),
        31,
        "a damaged key file is left alone, not overwritten"
    );
    let _ = std::fs::remove_file(&path);
}
