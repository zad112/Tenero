//! Version 3 onion addresses (`docs/TOR_PLAN.md`): `xxxx…xxxx.onion:PORT`, the 56 characters being base32 (RFC 4648,
//! lower case) of 35 bytes: the service's public key 32, a checksum 2, the version 1 (always 3). Tenero reads and writes
//! them and checks their form and version; it does NOT check the checksum (that needs SHA3-256, a dependency it does not
//! have): Tor checks it when the address is dialled, and refuses a wrong one.

/// An onion address's bytes on the wire.
pub const ONION_LEN: usize = 35;

/// How the transport names a peer that reached this node through its `.onion` service (`NetConfig::onion_listen`): this,
/// then a number of its own. Such peers all come from 127.0.0.1, so each is its own host (for limits and bans), and the
/// node knows the connection is through Tor.
pub const ONION_INBOUND: &str = "onion-in:";

const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// The host and port of `host:port` (an onion host, or anything without a colon in it).
pub fn split_host_port(addr: &str) -> Option<(&str, u16)> {
    let (host, port) = addr.rsplit_once(':')?;
    if host.contains(':') {
        return None;
    }
    Some((host, port.parse().ok()?))
}

/// Whether `addr` is `something.onion:PORT` (well formed or not: see [`parse_onion`]).
pub fn is_onion(addr: &str) -> bool {
    split_host_port(addr).is_some_and(|(h, _)| h.to_ascii_lowercase().ends_with(".onion"))
}

/// The 35 bytes and the port of a version 3 onion address `xxxx.onion:PORT`, or `None` if it is not one (the wrong length,
/// a character outside base32, a version other than 3, or port 0).
pub fn parse_onion(addr: &str) -> Option<([u8; ONION_LEN], u16)> {
    let (host, port) = split_host_port(addr)?;
    let host = host.to_ascii_lowercase();
    let name = host.strip_suffix(".onion")?;
    if name.len() != 56 || port == 0 {
        return None;
    }
    let mut out = [0u8; ONION_LEN];
    let (mut acc, mut bits, mut n) = (0u64, 0u32, 0usize);
    for c in name.bytes() {
        let v = ALPHABET.iter().position(|a| *a == c)? as u64;
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out[n] = (acc >> bits) as u8;
            n += 1;
            acc &= (1 << bits) - 1;
        }
    }
    // 56 x 5 = 280 bits = 35 bytes exactly, nothing left over
    if n != ONION_LEN || bits != 0 || out[ONION_LEN - 1] != 3 {
        return None;
    }
    Some((out, port))
}

/// `xxxx.onion:PORT` from the 35 bytes and the port.
pub fn onion_addr(bytes: &[u8; ONION_LEN], port: u16) -> String {
    let mut s = String::with_capacity(56 + 6 + 6);
    let (mut acc, mut bits) = (0u64, 0u32);
    for b in bytes {
        acc = (acc << 8) | u64::from(*b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            s.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
        acc &= (1 << bits) - 1;
    }
    format!("{s}.onion:{port}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Tor Project's own address (a real version 3 onion address, so its form is right).
    const TOR: &str = "2gzyxa5ihm7nsggfxnu52rck2vv4rvmdlkiu3zzui5du4xyclen53wid.onion:443";

    #[test]
    fn a_real_address_reads_and_writes_back() {
        let (b, port) = parse_onion(TOR).unwrap();
        assert_eq!((b[34], port), (3, 443));
        assert_eq!(onion_addr(&b, port), TOR);
        // upper case is the same address, written back in lower case
        let up = TOR.to_ascii_uppercase().replace(".ONION", ".onion");
        assert_eq!(onion_addr(&parse_onion(&up).unwrap().0, 443), TOR);
        assert!(is_onion(TOR) && !is_onion("1.2.3.4:5") && !is_onion("[::1]:5"));
    }

    #[test]
    fn what_is_not_a_version_3_address_is_refused() {
        let name = TOR.split('.').next().unwrap();
        for bad in [
            format!("{}.onion:443", &name[..55]),    // too short
            format!("{name}a.onion:443"),            // too long
            format!("{}1.onion:443", &name[..55]),   // '1' is not base32
            format!("{name}.onion:0"),               // no port
            format!("{name}.onion"),                 // no port at all
            format!("{name}.com:443"),               // not .onion
            "expyuzz4wqqyqhjn.onion:80".to_string(), // a version 2 address (retired by Tor)
        ] {
            assert!(parse_onion(&bad).is_none(), "{bad}");
        }
        // version byte not 3
        let (mut b, _) = parse_onion(TOR).unwrap();
        b[34] = 2;
        assert!(parse_onion(&onion_addr(&b, 1)).is_none());
    }

    #[test]
    fn every_byte_string_round_trips() {
        for seed in 0u8..64 {
            let mut b = [0u8; ONION_LEN];
            for (i, x) in b.iter_mut().enumerate() {
                *x = seed.wrapping_mul(31).wrapping_add(i as u8 * 7);
            }
            b[34] = 3;
            let a = onion_addr(&b, 1 + u16::from(seed));
            assert_eq!(parse_onion(&a), Some((b, 1 + u16::from(seed))), "{a}");
        }
    }
}
