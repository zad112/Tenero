//! A SOCKS5 client (RFC 1928, with the username/password of RFC 1929), enough to reach a peer through Tor
//! (`docs/TOR_PLAN.md`). Tor listens for it on `127.0.0.1:9050` (the Tor Browser on 9150). The connection made is a plain
//! byte stream to the peer; Tenero's Noise channel runs inside it as on a direct connection. This is not cryptography.
//!
//! Every connection is given its own username and password (`isolation`): Tor puts streams with different credentials on
//! different circuits (its `IsolateSOCKSAuth`, on by default), so two of a node's peers cannot see that they share one.

use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Duration;

use crate::onion::split_host_port;

fn err(why: impl Into<String>) -> io::Error {
    io::Error::other(why.into())
}

/// What a SOCKS5 reply code means (Tor's own codes for onion services included).
fn reply_text(code: u8) -> &'static str {
    match code {
        1 => "the proxy failed",
        2 => "the proxy's rules refuse it",
        3 => "the network is unreachable",
        4 => "the host is unreachable",
        5 => "the connection was refused",
        6 => "the connection timed out (TTL expired)",
        7 => "the proxy does not support CONNECT",
        8 => "the proxy does not support this kind of address",
        0xF0 => "the onion service descriptor cannot be found (it is not online, or the address is wrong)",
        0xF1 => "the onion service descriptor is invalid",
        0xF2 => "the onion service introduction failed",
        0xF3 => "the onion service rendezvous failed",
        0xF4 => "the onion service needs client authorisation",
        0xF5 => "the onion service client authorisation was wrong",
        0xF6 => "the onion address is invalid (a bad checksum or version)",
        0xF7 => "the onion service introduction timed out",
        _ => "the proxy refused it",
    }
}

/// Opens a connection to `target` (`ip:port`, `[v6]:port`, or `host:port` such as an onion address) through the SOCKS5
/// proxy at `proxy`, with `isolation` as its username and password (at most 255 bytes; empty for none). `timeout` bounds
/// each step. The stream is returned with no read or write timeout set.
pub fn connect(
    proxy: SocketAddr,
    target: &str,
    isolation: &str,
    timeout: Duration,
) -> io::Result<TcpStream> {
    let request = connect_request(target)?;
    if isolation.len() > 255 {
        return Err(err("the SOCKS username is longer than 255 bytes"));
    }
    let mut s = TcpStream::connect_timeout(&proxy, timeout)
        .map_err(|e| err(format!("cannot reach the Tor proxy at {proxy}: {e}")))?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    // the greeting: one method, username/password (or none, when there is nothing to isolate with)
    let method = if isolation.is_empty() { 0x00 } else { 0x02 };
    s.write_all(&[5, 1, method])?;
    let mut two = [0u8; 2];
    s.read_exact(&mut two)?;
    if two[0] != 5 {
        return Err(err(format!("{proxy} is not a SOCKS5 proxy")));
    }
    match two[1] {
        0x00 => {}
        0x02 if method == 0x02 => {
            let mut auth = vec![1, isolation.len() as u8];
            auth.extend_from_slice(isolation.as_bytes());
            auth.push(isolation.len() as u8);
            auth.extend_from_slice(isolation.as_bytes());
            s.write_all(&auth)?;
            s.read_exact(&mut two)?;
            if two[1] != 0 {
                return Err(err("the proxy refused the SOCKS username"));
            }
        }
        _ => {
            return Err(err(
                "the proxy accepts none of the ways offered to talk to it",
            ))
        }
    }
    s.write_all(&request)?;
    let mut head = [0u8; 4];
    s.read_exact(&mut head)?;
    if head[0] != 5 {
        return Err(err("a malformed SOCKS5 reply"));
    }
    if head[1] != 0 {
        return Err(err(format!(
            "Tor could not reach {target}: {}",
            reply_text(head[1])
        )));
    }
    // the address the proxy bound: read and thrown away
    let rest = match head[3] {
        1 => 4 + 2,
        4 => 16 + 2,
        3 => {
            let mut n = [0u8; 1];
            s.read_exact(&mut n)?;
            usize::from(n[0]) + 2
        }
        _ => return Err(err("a malformed SOCKS5 reply")),
    };
    let mut skip = vec![0u8; rest];
    s.read_exact(&mut skip)?;
    s.set_read_timeout(None)?;
    s.set_write_timeout(None)?;
    Ok(s)
}

/// The CONNECT request for `target`.
fn connect_request(target: &str) -> io::Result<Vec<u8>> {
    let mut r = vec![5, 1, 0];
    if let Ok(sa) = target.parse::<SocketAddr>() {
        match sa.ip() {
            IpAddr::V4(a) => {
                r.push(1);
                r.extend_from_slice(&a.octets());
            }
            IpAddr::V6(a) => {
                r.push(4);
                r.extend_from_slice(&a.octets());
            }
        }
        r.extend_from_slice(&sa.port().to_be_bytes());
        return Ok(r);
    }
    let (host, port) =
        split_host_port(target).ok_or_else(|| err(format!("`{target}` is not host:port")))?;
    if host.is_empty() || host.len() > 255 {
        return Err(err(format!(
            "`{target}`: the host name is not 1 to 255 bytes"
        )));
    }
    r.push(3);
    r.push(host.len() as u8);
    r.extend_from_slice(host.as_bytes());
    r.extend_from_slice(&port.to_be_bytes());
    Ok(r)
}

/// A fresh username for one connection: Tor gives each its own circuit.
pub fn isolation_name(counter: u64, salt: u64) -> String {
    format!("tenero-{salt:016x}-{counter}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// A tiny SOCKS5 proxy for the tests: it speaks the protocol, records what it was asked, and then either refuses with
    /// `refuse` or connects to `forward` (whatever the target was) and pipes bytes both ways.
    /// What the test proxy saw: the CONNECT request, and the username.
    type Seen = thread::JoinHandle<(Vec<u8>, Vec<u8>)>;

    fn proxy(refuse: Option<u8>, forward: SocketAddr) -> (SocketAddr, Seen) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = l.local_addr().unwrap();
        let h = thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut g = [0u8; 3];
            c.read_exact(&mut g).unwrap();
            let mut auth = Vec::new();
            if g[2] == 2 {
                c.write_all(&[5, 2]).unwrap();
                let mut v = [0u8; 2];
                c.read_exact(&mut v).unwrap();
                let mut user = vec![0u8; v[1] as usize];
                c.read_exact(&mut user).unwrap();
                let mut pl = [0u8; 1];
                c.read_exact(&mut pl).unwrap();
                let mut pass = vec![0u8; pl[0] as usize];
                c.read_exact(&mut pass).unwrap();
                assert_eq!(user, pass);
                auth = user;
                c.write_all(&[1, 0]).unwrap();
            } else {
                c.write_all(&[5, 0]).unwrap();
            }
            let mut head = [0u8; 4];
            c.read_exact(&mut head).unwrap();
            let mut req = head.to_vec();
            let n = match head[3] {
                1 => 6,
                4 => 18,
                _ => {
                    let mut l = [0u8; 1];
                    c.read_exact(&mut l).unwrap();
                    req.push(l[0]);
                    l[0] as usize + 2
                }
            };
            let mut body = vec![0u8; n];
            c.read_exact(&mut body).unwrap();
            req.extend_from_slice(&body);
            if let Some(code) = refuse {
                c.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap();
                return (req, auth);
            }
            c.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 1]).unwrap();
            let mut up = TcpStream::connect(forward).unwrap();
            let mut down = c.try_clone().unwrap();
            let mut up2 = up.try_clone().unwrap();
            let t = thread::spawn(move || {
                let _ = io::copy(&mut down, &mut up2);
            });
            let _ = io::copy(&mut up, &mut c);
            let _ = t.join();
            (req, auth)
        });
        (at, h)
    }

    /// An echo server: what it reads, it writes back.
    fn echo() -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = l.local_addr().unwrap();
        thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut buf = [0u8; 64];
            let n = c.read(&mut buf).unwrap();
            c.write_all(&buf[..n]).unwrap();
        });
        at
    }

    const ONION: &str = "2gzyxa5ihm7nsggfxnu52rck2vv4rvmdlkiu3zzui5du4xyclen53wid.onion:38353";

    #[test]
    fn an_onion_address_is_reached_through_the_proxy_by_name_with_its_own_username() {
        let (p, h) = proxy(None, echo());
        let mut s = connect(p, ONION, "tenero-1", Duration::from_secs(5)).unwrap();
        s.write_all(b"hello through tor").unwrap();
        let mut back = [0u8; 17];
        s.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"hello through tor");
        drop(s);
        let (req, auth) = h.join().unwrap();
        // CONNECT, a domain name (the proxy, not this computer, resolves it), its length, the name, the port
        let host = ONION.split(':').next().unwrap();
        assert_eq!(&req[..5], &[5, 1, 0, 3, host.len() as u8]);
        assert_eq!(&req[5..5 + host.len()], host.as_bytes());
        assert_eq!(&req[5 + host.len()..], &38353u16.to_be_bytes());
        assert_eq!(auth, b"tenero-1");
    }

    #[test]
    fn an_ip_address_is_sent_as_an_address() {
        let (p, h) = proxy(None, echo());
        let mut s = connect(p, "203.0.113.9:38353", "", Duration::from_secs(5)).unwrap();
        s.write_all(b"x").unwrap();
        let mut one = [0u8; 1];
        s.read_exact(&mut one).unwrap();
        drop(s);
        let (req, auth) = h.join().unwrap();
        assert_eq!(req, [5, 1, 0, 1, 203, 0, 113, 9, 0x95, 0xD1]);
        assert!(auth.is_empty());
        let (p, h) = proxy(None, echo());
        let mut s = connect(p, "[2001:db8::1]:8", "", Duration::from_secs(5)).unwrap();
        s.write_all(b"x").unwrap();
        s.read_exact(&mut one).unwrap();
        drop(s);
        let (req, _) = h.join().unwrap();
        assert_eq!(&req[..4], &[5, 1, 0, 4]);
        assert_eq!(req.len(), 4 + 16 + 2);
    }

    #[test]
    fn a_refusal_says_why_and_no_proxy_says_so() {
        let (p, h) = proxy(Some(0xF0), echo());
        let e = connect(p, ONION, "x", Duration::from_secs(5)).unwrap_err();
        assert!(e.to_string().contains("descriptor cannot be found"), "{e}");
        h.join().unwrap();
        // nothing listening where the proxy should be
        let free = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let e = connect(free, ONION, "x", Duration::from_secs(2)).unwrap_err();
        assert!(e.to_string().contains("cannot reach the Tor proxy"), "{e}");
        assert!(connect_request("no-port").is_err());
        assert!(connect_request(&format!("{}:1", "a".repeat(256))).is_err());
    }
}
