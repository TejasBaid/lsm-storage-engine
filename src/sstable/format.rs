//! Block handles, block trailers and the fixed-size table footer.

use std::fs::File;
use std::io;

use crate::coding::{
    Decoder, decode_fixed32, decode_fixed64, put_fixed32, put_fixed64, put_varint64,
};
use crate::error::{Error, Result};

/// `compression type (u8) | crc32 (u32 LE)` appended to every block.
pub const BLOCK_TRAILER_SIZE: usize = 5;
/// Size of the table footer.
pub const FOOTER_SIZE: usize = 48;
/// Magic number terminating every table ("LSMTABLE" read as LE bytes).
pub const TABLE_MAGIC: u64 = u64::from_le_bytes(*b"LSMTABLE");
/// Current table format version.
pub const FORMAT_VERSION: u32 = 1;

/// Block compression codes. Only `None` is implemented; the byte is
/// reserved so compression can be added without a format change.
pub const COMPRESSION_NONE: u8 = 0;

/// Location of a block within a table file (contents only, trailer excluded).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockHandle {
    /// Byte offset of the block contents.
    pub offset: u64,
    /// Length of the block contents.
    pub size: u64,
}

impl BlockHandle {
    /// Appends the varint encoding used inside index blocks.
    pub fn encode_to(&self, dst: &mut Vec<u8>) {
        put_varint64(dst, self.offset);
        put_varint64(dst, self.size);
    }

    /// Decodes a handle from an index-block value.
    pub fn decode(src: &[u8]) -> Result<Self> {
        let mut d = Decoder::new(src);
        Ok(BlockHandle {
            offset: d.varint64()?,
            size: d.varint64()?,
        })
    }
}

/// The fixed-size table footer.
///
/// ```text
/// footer := filter_offset u64 | filter_size u64 | index_offset u64 | index_size u64
///         | format_version u32 | crc32(previous 36 bytes) u32 | magic u64
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footer {
    /// Bloom filter block (size 0 if the table has no filter).
    pub filter: BlockHandle,
    /// Index block.
    pub index: BlockHandle,
}

impl Footer {
    /// Serializes the footer.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(FOOTER_SIZE);
        put_fixed64(&mut out, self.filter.offset);
        put_fixed64(&mut out, self.filter.size);
        put_fixed64(&mut out, self.index.offset);
        put_fixed64(&mut out, self.index.size);
        put_fixed32(&mut out, FORMAT_VERSION);
        let crc = crc32fast::hash(&out);
        put_fixed32(&mut out, crc);
        put_fixed64(&mut out, TABLE_MAGIC);
        debug_assert_eq!(out.len(), FOOTER_SIZE);
        out
    }

    /// Parses and validates a footer.
    pub fn decode(src: &[u8]) -> Result<Self> {
        if src.len() != FOOTER_SIZE {
            return Err(Error::corruption("footer has wrong size"));
        }
        if decode_fixed64(&src[40..]) != TABLE_MAGIC {
            return Err(Error::corruption("bad table magic number (not an SSTable)"));
        }
        if decode_fixed32(&src[36..]) != crc32fast::hash(&src[..36]) {
            return Err(Error::corruption("footer checksum mismatch"));
        }
        let version = decode_fixed32(&src[32..]);
        if version != FORMAT_VERSION {
            return Err(Error::corruption(format!(
                "unsupported table format version {version}"
            )));
        }
        let h = |i: usize| BlockHandle {
            offset: decode_fixed64(&src[i..]),
            size: decode_fixed64(&src[i + 8..]),
        };
        Ok(Footer {
            filter: h(0),
            index: h(16),
        })
    }
}

/// Reads exactly `buf.len()` bytes at `offset` without moving a shared
/// cursor, so concurrent readers can share one file handle.
pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let (mut buf, mut offset) = (buf, offset);
        while !buf.is_empty() {
            match file.seek_read(buf, offset) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Reads a block and its trailer, verifying the checksum if requested.
pub fn read_block(
    file: &File,
    file_size: u64,
    handle: BlockHandle,
    verify_checksum: bool,
) -> Result<Vec<u8>> {
    let end = handle
        .offset
        .checked_add(handle.size)
        .and_then(|e| e.checked_add(BLOCK_TRAILER_SIZE as u64));
    if end.is_none_or(|e| e > file_size) {
        return Err(Error::corruption(format!(
            "block handle {handle:?} extends past end of file ({file_size} bytes)"
        )));
    }
    let n = handle.size as usize;
    let mut buf = vec![0u8; n + BLOCK_TRAILER_SIZE];
    read_exact_at(file, &mut buf, handle.offset)?;
    let compression = buf[n];
    if verify_checksum {
        let expected = decode_fixed32(&buf[n + 1..]);
        if crc32fast::hash(&buf[..n + 1]) != expected {
            return Err(Error::corruption(format!(
                "block checksum mismatch at offset {}",
                handle.offset
            )));
        }
    }
    if compression != COMPRESSION_NONE {
        return Err(Error::corruption(format!(
            "unknown block compression type {compression}"
        )));
    }
    buf.truncate(n);
    Ok(buf)
}

/// Appends the trailer for `contents` (uncompressed) to `dst`.
pub fn block_trailer(contents: &[u8]) -> [u8; BLOCK_TRAILER_SIZE] {
    let mut h = crc32fast::Hasher::new();
    h.update(contents);
    h.update(&[COMPRESSION_NONE]);
    let crc = h.finalize().to_le_bytes();
    [COMPRESSION_NONE, crc[0], crc[1], crc[2], crc[3]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn footer_roundtrip_and_validation() {
        let f = Footer {
            filter: BlockHandle {
                offset: 100,
                size: 20,
            },
            index: BlockHandle {
                offset: 125,
                size: 300,
            },
        };
        let enc = f.encode();
        assert_eq!(Footer::decode(&enc).unwrap(), f);

        let mut bad_magic = enc.clone();
        bad_magic[47] ^= 1;
        assert!(Footer::decode(&bad_magic).is_err());
        let mut bad_crc = enc.clone();
        bad_crc[3] ^= 1;
        assert!(Footer::decode(&bad_crc).is_err());
    }

    proptest! {
        #[test]
        fn handle_roundtrip(offset in any::<u64>(), size in any::<u64>()) {
            let h = BlockHandle { offset, size };
            let mut buf = Vec::new();
            h.encode_to(&mut buf);
            prop_assert_eq!(BlockHandle::decode(&buf).unwrap(), h);
        }

        #[test]
        fn footer_decode_never_panics(data in proptest::collection::vec(any::<u8>(), FOOTER_SIZE)) {
            let _ = Footer::decode(&data);
        }
    }
}
