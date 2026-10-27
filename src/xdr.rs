//! XDR (RFC 4506) encoding and decoding: big-endian, every item padded to a
//! 4-byte boundary. Decoding returns `None` on any short or malformed input,
//! which the RPC layer turns into GARBAGE_ARGS.

/// XDR encoder over a growable buffer.
#[derive(Default)]
pub struct Xdr {
    buf: Vec<u8>,
}

impl Xdr {
    pub fn new() -> Self {
        Self { buf: Vec::with_capacity(256) }
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn i32(&mut self, v: i32) {
        self.u32(v as u32);
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn bool(&mut self, v: bool) {
        self.u32(v as u32);
    }

    /// Variable-length opaque or string: length prefix, bytes, pad.
    pub fn opaque(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.fixed(b);
    }

    /// Fixed-length opaque: bytes and pad, no length prefix.
    pub fn fixed(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
        self.buf.resize(self.buf.len() + pad(b.len()), 0);
    }

    /// Append already-encoded XDR.
    pub fn raw(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// Bytes of padding after an item of `len` bytes.
pub fn pad(len: usize) -> usize {
    (4 - (len % 4)) % 4
}

/// Encoded size of a variable-length opaque of `len` bytes.
pub fn opaque_size(len: usize) -> usize {
    4 + len + pad(len)
}

/// XDR decode cursor over a borrowed message.
pub struct Cur<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.b.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    pub fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }

    pub fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    pub fn bool(&mut self) -> Option<bool> {
        self.u32().map(|v| v != 0)
    }

    /// Variable-length opaque or string of at most `max` bytes.
    pub fn opaque(&mut self, max: usize) -> Option<&'a [u8]> {
        let len = self.u32()? as usize;
        if len > max {
            return None;
        }
        self.fixed(len)
    }

    /// Fixed-length opaque of `len` bytes. A missing trailing pad at the very
    /// end of the message is tolerated; some old clients omit it.
    pub fn fixed(&mut self, len: usize) -> Option<&'a [u8]> {
        let s = self.take(len)?;
        self.pos = (self.pos + pad(len)).min(self.b.len());
        Some(s)
    }

    pub fn remaining(&self) -> usize {
        self.b.len() - self.pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut x = Xdr::new();
        x.u32(0xdead_beef);
        x.u64(0x0102_0304_0506_0708);
        x.opaque(b"abc");
        x.bool(true);
        let bytes = x.into_bytes();
        assert_eq!(bytes.len(), 4 + 8 + 8 + 4);
        let mut c = Cur::new(&bytes);
        assert_eq!(c.u32(), Some(0xdead_beef));
        assert_eq!(c.u64(), Some(0x0102_0304_0506_0708));
        assert_eq!(c.opaque(16), Some(&b"abc"[..]));
        assert_eq!(c.bool(), Some(true));
        assert_eq!(c.u32(), None);
    }

    #[test]
    fn opaque_respects_max_and_length() {
        let mut x = Xdr::new();
        x.opaque(b"hello");
        let b = x.into_bytes();
        assert!(Cur::new(&b).opaque(4).is_none());
        // A length prefix that runs past the end of the message.
        assert!(Cur::new(&[0, 0, 0, 9, 1, 2]).opaque(100).is_none());
    }
}
