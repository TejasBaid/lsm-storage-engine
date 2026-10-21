//! Little-endian fixed-width and LEB128-style varint encodings, plus a
//! bounds-checked [`Decoder`] cursor used by every on-disk parser.
//!
//! All decoders return [`Error::Corruption`] instead of panicking on
//! malformed input: the bytes may come from a damaged disk.

use crate::error::{Error, Result};

pub fn put_fixed32(dst: &mut Vec<u8>, v: u32) {
    dst.extend_from_slice(&v.to_le_bytes());
}

pub fn put_fixed64(dst: &mut Vec<u8>, v: u64) {
    dst.extend_from_slice(&v.to_le_bytes());
}

/// Decodes a little-endian `u32` from the first four bytes of `src`.
///
/// # Panics
/// Panics if `src.len() < 4`; callers must check bounds first.
pub fn decode_fixed32(src: &[u8]) -> u32 {
    u32::from_le_bytes(src[..4].try_into().expect("slice of length 4"))
}

/// Decodes a little-endian `u64` from the first eight bytes of `src`.
///
/// # Panics
/// Panics if `src.len() < 8`; callers must check bounds first.
pub fn decode_fixed64(src: &[u8]) -> u64 {
    u64::from_le_bytes(src[..8].try_into().expect("slice of length 8"))
}

pub fn put_varint32(dst: &mut Vec<u8>, v: u32) {
    put_varint64(dst, u64::from(v));
}

/// Appends `v` as a base-128 varint (7 bits per byte, MSB = continuation).
pub fn put_varint64(dst: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        dst.push((v as u8) | 0x80);
        v >>= 7;
    }
    dst.push(v as u8);
}

/// Decodes a varint from the front of `src`, returning `(value, bytes_read)`.
/// Returns `None` if the input is truncated or longer than 10 bytes.
pub fn decode_varint64(src: &[u8]) -> Option<(u64, usize)> {
    let mut result = 0u64;
    for (i, &byte) in src.iter().enumerate().take(10) {
        result |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }
    }
    None
}

/// Decodes a varint that must fit in a `u32`.
pub fn decode_varint32(src: &[u8]) -> Option<(u32, usize)> {
    // Fast path: single-byte values dominate block entry headers.
    if let Some(&b) = src.first()
        && b < 0x80
    {
        return Some((u32::from(b), 1));
    }
    let (v, n) = decode_varint64(src)?;
    if n > 5 {
        return None;
    }
    u32::try_from(v).ok().map(|v| (v, n))
}

pub fn put_length_prefixed_slice(dst: &mut Vec<u8>, s: &[u8]) {
    put_varint64(dst, s.len() as u64);
    dst.extend_from_slice(s);
}

/// A forward-only, bounds-checked reader over a byte slice.
pub struct Decoder<'a> {
    buf: &'a [u8],
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Decoder { buf }
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    fn truncated(what: &str) -> Error {
        Error::corruption(format!("truncated {what}"))
    }

    pub fn u8(&mut self) -> Result<u8> {
        let (&b, rest) = self
            .buf
            .split_first()
            .ok_or_else(|| Self::truncated("byte"))?;
        self.buf = rest;
        Ok(b)
    }

    pub fn varint64(&mut self) -> Result<u64> {
        let (v, n) = decode_varint64(self.buf).ok_or_else(|| Self::truncated("varint64"))?;
        self.buf = &self.buf[n..];
        Ok(v)
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() < n {
            return Err(Self::truncated("byte string"));
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    pub fn length_prefixed(&mut self) -> Result<&'a [u8]> {
        let len = self.varint64()?;
        let len = usize::try_from(len).map_err(|_| Self::truncated("length-prefixed slice"))?;
        self.bytes(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn varint_boundaries() {
        for v in [
            0u64,
            1,
            127,
            128,
            255,
            16383,
            16384,
            u32::MAX as u64,
            u64::MAX,
        ] {
            let mut buf = Vec::new();
            put_varint64(&mut buf, v);
            assert_eq!(decode_varint64(&buf), Some((v, buf.len())));
        }
    }

    #[test]
    fn varint32_rejects_overflow() {
        let mut buf = Vec::new();
        put_varint64(&mut buf, u64::from(u32::MAX) + 1);
        assert_eq!(decode_varint32(&buf), None);
    }

    #[test]
    fn truncated_varint_is_none() {
        assert_eq!(decode_varint64(&[0x80, 0x80]), None);
        assert_eq!(decode_varint64(&[]), None);
    }

    #[test]
    fn decoder_reports_truncation() {
        let mut d = Decoder::new(&[1, 2, 3]);
        assert!(d.bytes(4).is_err());
        let mut d = Decoder::new(&[5, b'a']);
        assert!(d.length_prefixed().is_err());
    }

    proptest! {
        #[test]
        fn varint_roundtrip(v in any::<u64>()) {
            let mut buf = Vec::new();
            put_varint64(&mut buf, v);
            prop_assert_eq!(decode_varint64(&buf), Some((v, buf.len())));
        }

        #[test]
        fn decoder_roundtrip(a in any::<u32>(), b in any::<u64>(), s in proptest::collection::vec(any::<u8>(), 0..64)) {
            let mut buf = Vec::new();
            put_fixed32(&mut buf, a);
            put_varint64(&mut buf, b);
            put_length_prefixed_slice(&mut buf, &s);
            put_fixed64(&mut buf, b);
            let mut d = Decoder::new(&buf);
            prop_assert_eq!(decode_fixed32(d.bytes(4).unwrap()), a);
            prop_assert_eq!(d.varint64().unwrap(), b);
            prop_assert_eq!(d.length_prefixed().unwrap(), &s[..]);
            prop_assert_eq!(decode_fixed64(d.bytes(8).unwrap()), b);
            prop_assert!(d.is_empty());
        }

        #[test]
        fn decoder_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..32)) {
            let mut d = Decoder::new(&bytes);
            while d.varint64().is_ok() || d.u8().is_ok() {}
        }
    }
}
