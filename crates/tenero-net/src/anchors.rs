//! Anchor peers: the saved form (M9, threat model C1).
//!
//! **What an anchor is.** A node that restarts forgets who it was connected to, and begins again from its address book and its
//! seeds: exactly the moment an eclipse attacker waits for. Before a node stops (and every time its state is saved) it
//! remembers a few of its **outbound** peers that it has been connected to for a while, chosen by the node itself and not by whoever
//! connected to it, and when it starts again it dials those **first**, before anything the address book or the seeds offer.
//! If even one of them is honest, the node is not alone with an attacker's peers. This is the "anchor connections" idea of
//! Bitcoin Core (its `anchors.dat`).
//!
//! **What this file is.** Only the encoding of the list, checksummed like the address book and the ban list (a damaged list is
//! refused whole). The choosing and the dialling are the engine's (`engine.rs`).
//!
//! **Limits.** An anchor is as good as the connection that was chosen: a node that was already eclipsed when it saved has
//! anchors that belong to the attacker. Anchors are tried once each; one that no longer answers is not remembered again.

use std::net::SocketAddr;

use tenero_core::hash::sha256;

/// The most anchors a list may hold (the engine keeps `EngineConfig::anchor_count`, which is at most this).
pub const MAX_ANCHORS: usize = 8;

/// `TAN1 | count u32 | (length u8, address text)* | checksum 4`.
pub fn to_bytes(addrs: &[String]) -> Vec<u8> {
    let mut out = b"TAN1".to_vec();
    out.extend_from_slice(&(addrs.len() as u32).to_le_bytes());
    for a in addrs {
        out.push(a.len() as u8);
        out.extend_from_slice(a.as_bytes());
    }
    let sum = sha256(&[&out]);
    out.extend_from_slice(&sum[..4]);
    out
}

/// Strict: a damaged, truncated or padded list, one over [`MAX_ANCHORS`], or one with an entry that is not `ip:port`
/// (or has port 0), is refused.
pub fn from_bytes(data: &[u8]) -> Result<Vec<String>, String> {
    if data.len() < 12 || &data[..4] != b"TAN1" {
        return Err("not an anchor list".into());
    }
    let (body, sum) = data.split_at(data.len() - 4);
    if sha256(&[body])[..4] != *sum {
        return Err("the anchor list is damaged (checksum)".into());
    }
    let n = u32::from_le_bytes(body[4..8].try_into().unwrap()) as usize;
    if n > MAX_ANCHORS {
        return Err(format!("{n} anchors, at most {MAX_ANCHORS}"));
    }
    let mut pos = 8;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let len = *body.get(pos).ok_or("truncated")? as usize;
        pos += 1;
        let bytes = body.get(pos..pos + len).ok_or("truncated")?;
        pos += len;
        let text = std::str::from_utf8(bytes).map_err(|_| "an anchor is not text")?;
        match text.parse::<SocketAddr>() {
            Ok(sa) if sa.port() != 0 => out.push(text.to_string()),
            _ => return Err(format!("{text:?} is not an address and port")),
        }
    }
    if pos != body.len() {
        return Err("trailing bytes".into());
    }
    Ok(out)
}
