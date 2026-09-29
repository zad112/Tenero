//! The canonical binary format of version 2 (`docs/CONSENSUS_V2.md` section 4).
//!
//! Integers are fixed width and little-endian (no varints); fixed-size byte arrays are written as
//! they are; variable byte strings and lists carry a `u32` length or count that is checked against
//! its maximum **before** anything is read or allocated. Decoding is strict: trailing bytes are an
//! error, and one value has exactly one encoding.

/// Why decoding failed. The four kinds are the ones the vectors name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    ShortRead,
    TrailingBytes,
    CountOutOfRange,
    LengthOverMaximum,
}

impl DecodeError {
    /// The name used in `tests/vectors/v2_serialization.json`.
    pub fn as_str(self) -> &'static str {
        match self {
            DecodeError::ShortRead => "short read",
            DecodeError::TrailingBytes => "trailing bytes",
            DecodeError::CountOutOfRange => "count out of range",
            DecodeError::LengthOverMaximum => "length over maximum",
        }
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for DecodeError {}

/// Why an object could not be encoded: it is outside the limits a decoder would enforce, so it could
/// never be read back. Encoding refuses it instead of writing bytes that no node would accept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    CountOutOfRange,
    LengthOverMaximum,
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            EncodeError::CountOutOfRange => "count out of range",
            EncodeError::LengthOverMaximum => "length over maximum",
        })
    }
}

impl std::error::Error for EncodeError {}

/// Reads from a byte slice, never past its end.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, pos: 0 }
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let rest = &self.data[self.pos..];
        if n > rest.len() {
            return Err(DecodeError::ShortRead);
        }
        self.pos += n;
        Ok(&rest[..n])
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// A byte string with a `u32` length. The length is checked against `max` before reading.
    pub fn var(&mut self, max: usize) -> Result<Vec<u8>, DecodeError> {
        let n = self.u32()? as usize;
        if n > max {
            return Err(DecodeError::LengthOverMaximum);
        }
        Ok(self.take(n)?.to_vec())
    }

    /// A list count in `min..=max`, checked before any element is read or any memory reserved.
    pub fn count(&mut self, min: usize, max: usize) -> Result<usize, DecodeError> {
        let n = self.u32()? as usize;
        if n < min || n > max {
            return Err(DecodeError::CountOutOfRange);
        }
        Ok(n)
    }

    /// A list: a count in `min..=max`, then that many elements.
    pub fn list<T>(
        &mut self,
        min: usize,
        max: usize,
        mut item: impl FnMut(&mut Reader<'a>) -> Result<T, DecodeError>,
    ) -> Result<Vec<T>, DecodeError> {
        let n = self.count(min, max)?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(item(self)?);
        }
        Ok(out)
    }

    /// Succeeds only when every byte has been used.
    pub fn finish(self) -> Result<(), DecodeError> {
        if self.pos == self.data.len() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes)
        }
    }
}

/// Appends to a byte vector, refusing what a decoder would refuse.
#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Writer {
        Writer::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// A byte string with a `u32` length; at most `max` bytes.
    pub fn var(&mut self, bytes: &[u8], max: usize) -> Result<(), EncodeError> {
        if bytes.len() > max {
            return Err(EncodeError::LengthOverMaximum);
        }
        self.u32(u32::try_from(bytes.len()).map_err(|_| EncodeError::LengthOverMaximum)?);
        self.raw(bytes);
        Ok(())
    }

    /// A list count, which must be in `min..=max`.
    pub fn count(&mut self, n: usize, min: usize, max: usize) -> Result<(), EncodeError> {
        if n < min || n > max {
            return Err(EncodeError::CountOutOfRange);
        }
        self.u32(u32::try_from(n).map_err(|_| EncodeError::CountOutOfRange)?);
        Ok(())
    }
}

/// An object with a canonical wire form.
pub trait Wire: Sized {
    fn write(&self, w: &mut Writer) -> Result<(), EncodeError>;
    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError>;

    fn to_bytes(&self) -> Result<Vec<u8>, EncodeError> {
        let mut w = Writer::new();
        self.write(&mut w)?;
        Ok(w.into_bytes())
    }

    /// Strict: the input must be exactly one object.
    fn from_bytes(data: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(data);
        let v = Self::read(&mut r)?;
        r.finish()?;
        Ok(v)
    }
}
