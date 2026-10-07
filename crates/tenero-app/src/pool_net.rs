//! The encrypted connection of the pool protocol (`docs/POOL_PROTOCOL.md`): the Noise handshake with the prologue `tenero pool v1` and no
//! pre-shared key, and the two halves of the finished channel as `Read` and `Write` so that one thread can wait for the miner's messages
//! while another sends jobs. **Experimental and unaudited; nothing on any network it serves has value.**
//!
//! **Who is on the other end.** A pool is public, so there is no shared key. What a miner can do is **pin the pool's public key**: it is
//! given the key (a field of the app, `--pool-key`) and refuses a pool that proves any other, which stops a person who sits between the
//! miner and the pool. A pool cannot tell who a miner is, and does not try.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use tenero_net::noise::{
    handshake_initiator, handshake_responder, NodeKey, NoiseError, SecureReader, SecureWriter,
};

use crate::pool::{frame, frame_len, MAX_FRAME};

/// Bound into the handshake, so a pool port cannot be mistaken for any other Tenero port.
pub const PROLOGUE: &[u8] = b"tenero pool v1";
/// The default port of a pool (`docs/POOL_PROTOCOL.md`, decision 4).
pub const DEFAULT_PORT: u16 = 38335;

fn to_io(e: NoiseError) -> io::Error {
    match e {
        NoiseError::Io(e) => e,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

/// The receiving half: the socket and what decrypts it.
pub struct ReadHalf {
    stream: TcpStream,
    reader: SecureReader,
    buf: Vec<u8>,
    pos: usize,
}

/// The sending half.
pub struct WriteHalf {
    stream: TcpStream,
    writer: SecureWriter,
}

impl Read for ReadHalf {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.pos >= self.buf.len() {
            self.buf = self.reader.read_chunk(&mut self.stream).map_err(to_io)?;
            self.pos = 0;
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for WriteHalf {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.writer
            .write_all(&mut self.stream, data)
            .map_err(to_io)?;
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

impl ReadHalf {
    /// Changes how long a read may wait for the other side.
    pub fn set_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(d)
    }

    /// Closes the socket (both halves share it): a thread blocked in a read returns.
    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

impl WriteHalf {
    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

fn halves(
    stream: TcpStream,
    reader: SecureReader,
    writer: SecureWriter,
) -> io::Result<(ReadHalf, WriteHalf)> {
    let other = stream.try_clone()?;
    Ok((
        ReadHalf {
            stream,
            reader,
            buf: Vec::new(),
            pos: 0,
        },
        WriteHalf {
            stream: other,
            writer,
        },
    ))
}

/// Finishes the handshake on a connection a pool accepted, with the pool's long-term `key`. Returns the two halves and the miner's
/// static public key (which proves nothing about who the miner is).
pub fn accept(
    mut stream: TcpStream,
    key: &NodeKey,
    handshake_timeout: Duration,
    idle_timeout: Duration,
) -> Result<(ReadHalf, WriteHalf, [u8; 32]), String> {
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(handshake_timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    let s = handshake_responder(&mut stream, key, PROLOGUE).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(idle_timeout))
        .map_err(|e| e.to_string())?;
    let (r, w) = halves(stream, s.reader, s.writer).map_err(|e| e.to_string())?;
    Ok((r, w, s.remote_public))
}

/// Dials a pool and finishes the handshake. If `pinned` is given, the pool must prove exactly that public key, or the connection is
/// refused (a pool that proves another key is not the pool the miner was told about, or someone is in the middle).
pub fn connect(
    addr: SocketAddr,
    pinned: Option<&[u8; 32]>,
) -> Result<(ReadHalf, WriteHalf), String> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map_err(|e| format!("cannot reach the pool at {addr}: {e}"))?;
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    let me = NodeKey::generate();
    let s = handshake_initiator(&mut stream, &me, PROLOGUE).map_err(|e| {
        format!("the pool at {addr} did not finish the handshake: {e} (is that a pool port?)")
    })?;
    if let Some(p) = pinned {
        if &s.remote_public != p {
            return Err(format!(
                "the pool at {addr} proved a different key than the one this miner was given: it is not the pool you meant, or someone is between you and it. Not connecting."
            ));
        }
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(300)))
        .map_err(|e| e.to_string())?;
    halves(stream, s.reader, s.writer).map_err(|e| e.to_string())
}

/// Reads one message body: a length (1 to [`MAX_FRAME`], else an error and nothing more is read) and that many bytes.
pub fn read_message<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut h = [0u8; 4];
    r.read_exact(&mut h)?;
    let n = frame_len(h).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    debug_assert!(n <= MAX_FRAME);
    let mut body = Vec::with_capacity(n.min(64 * 1024));
    let mut left = n;
    let mut buf = [0u8; 16 * 1024];
    while left > 0 {
        let take = left.min(buf.len());
        r.read_exact(&mut buf[..take])?;
        body.extend_from_slice(&buf[..take]);
        left -= take;
    }
    Ok(body)
}

/// Writes one message body with its length.
pub fn write_message<W: Write>(w: &mut W, body: &[u8]) -> io::Result<()> {
    let f = frame(body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    w.write_all(&f)?;
    w.flush()
}
