// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Minimal ISO-BMFF box writer for CMAF fragments.
//!
//! A box is `size (u32) + type (fourcc) + body`. The `size` is patched in
//! when the box is closed, so call sites can write a nested structure
//! without knowing the final length up front.
//!
//! Full boxes prepend `version (u8) + flags (u24)` before the body
//! (ISO/IEC 14496-12 §4.2 `FullBox`).
//!
//! Large-size (`size == 1`, extended 64-bit size) boxes are not emitted —
//! CMAF fragments never approach the 4 GiB boundary.
//!
//! # Usage
//!
//! ```ignore
//! let mut buf = Vec::new();
//! {
//!     let mut b = BoxWriter::open(&mut buf, *b"mvhd");
//!     b.u8(0);           // version
//!     b.u24(0);          // flags
//!     b.u32(0);          // creation_time
//!     // ... more fields
//! } // Drop patches the size prefix.
//! ```

/// A scoped box writer. Holds a mutable reference into an output `Vec<u8>`
/// and remembers where its own size field starts. On drop, the size is
/// back-patched so the caller doesn't have to track lengths.
pub struct BoxWriter<'a> {
    buf: &'a mut Vec<u8>,
    start: usize,
}

impl<'a> BoxWriter<'a> {
    /// Open a new box: reserves 4 bytes for size, writes the fourcc, and
    /// returns a writer ready to accept body bytes.
    pub fn open(buf: &'a mut Vec<u8>, fourcc: [u8; 4]) -> Self {
        let start = buf.len();
        buf.extend_from_slice(&[0, 0, 0, 0]); // size placeholder
        buf.extend_from_slice(&fourcc);
        Self { buf, start }
    }

    /// Open a new `FullBox`: reserves 4 bytes for size, writes the fourcc,
    /// then the version + flags header (ISO/IEC 14496-12 §4.2).
    pub fn open_full(buf: &'a mut Vec<u8>, fourcc: [u8; 4], version: u8, flags: u32) -> Self {
        let mut b = Self::open(buf, fourcc);
        b.u8(version);
        b.u24(flags);
        b
    }

    /// Append a nested child box. The child borrows the same underlying
    /// buffer; its lifetime is scoped so parent writes are blocked while
    /// the child is open (Rust's borrow checker enforces this at compile
    /// time).
    pub fn child(&mut self, fourcc: [u8; 4]) -> BoxWriter<'_> {
        BoxWriter::open(self.buf, fourcc)
    }

    /// Append a nested child FullBox.
    pub fn child_full(&mut self, fourcc: [u8; 4], version: u8, flags: u32) -> BoxWriter<'_> {
        BoxWriter::open_full(self.buf, fourcc, version, flags)
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    /// 24-bit big-endian unsigned integer (for FullBox flags and
    /// `section_length`-style fields).
    pub fn u24(&mut self, v: u32) {
        debug_assert!(v < 0x0100_0000, "u24 overflow: {v}");
        self.buf.push(((v >> 16) & 0xFF) as u8);
        self.buf.push(((v >> 8) & 0xFF) as u8);
        self.buf.push((v & 0xFF) as u8);
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn bytes(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn fourcc(&mut self, fc: [u8; 4]) {
        self.buf.extend_from_slice(&fc);
    }

    /// Pad with `n` zero bytes. Used for reserved fields and the fixed-width
    /// name field in handlers / track identification atoms.
    pub fn zeros(&mut self, n: usize) {
        for _ in 0..n {
            self.buf.push(0);
        }
    }

    /// Current body length (excluding the 8-byte size+type header).
    /// Used by senc/saio writers in Phase 5.
    #[allow(dead_code)]
    pub fn body_len(&self) -> usize {
        self.buf.len() - self.start - 8
    }

    /// Absolute offset of the current write cursor from the start of `buf`.
    /// Used when a caller needs to record `saio`-style absolute offsets.
    pub fn cursor_pos(&self) -> usize {
        self.buf.len()
    }

    /// Absolute offset of the start of this box's size field. Useful when
    /// child boxes need to record offsets relative to their parent. Used
    /// by saio writers in Phase 5.
    #[allow(dead_code)]
    pub fn box_start(&self) -> usize {
        self.start
    }
}

impl<'a> Drop for BoxWriter<'a> {
    fn drop(&mut self) {
        let total = self.buf.len() - self.start;
        debug_assert!(total <= u32::MAX as usize, "box exceeds 4 GiB: {total}");
        let size_bytes = (total as u32).to_be_bytes();
        self.buf[self.start..self.start + 4].copy_from_slice(&size_bytes);
    }
}

/// Patch the u32 at `buf[offset..offset+4]` with `value` in big-endian.
/// Used after a box is closed to fix up forward references (e.g. the
/// `data_offset` in `trun` that points into the later `mdat`).
pub fn patch_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_box_has_size_8() {
        let mut buf = Vec::new();
        {
            let _b = BoxWriter::open(&mut buf, *b"ftyp");
        }
        assert_eq!(buf.len(), 8);
        assert_eq!(&buf[0..4], &[0, 0, 0, 8]); // size = 8
        assert_eq!(&buf[4..8], b"ftyp");
    }

    #[test]
    fn box_with_body_patches_size() {
        let mut buf = Vec::new();
        {
            let mut b = BoxWriter::open(&mut buf, *b"mdat");
            b.u32(0x11223344);
            b.u16(0xAABB);
        }
        // 8 header + 6 body = 14
        assert_eq!(buf.len(), 14);
        assert_eq!(&buf[0..4], &[0, 0, 0, 14]);
        assert_eq!(&buf[4..8], b"mdat");
        assert_eq!(&buf[8..12], &[0x11, 0x22, 0x33, 0x44]);
        assert_eq!(&buf[12..14], &[0xAA, 0xBB]);
    }

    #[test]
    fn full_box_writes_version_flags() {
        let mut buf = Vec::new();
        {
            let _b = BoxWriter::open_full(&mut buf, *b"mvhd", 1, 0x00ABCDEF);
        }
        assert_eq!(buf.len(), 12); // 8 header + 1 version + 3 flags
        assert_eq!(&buf[0..4], &[0, 0, 0, 12]);
        assert_eq!(&buf[4..8], b"mvhd");
        assert_eq!(buf[8], 1); // version
        assert_eq!(&buf[9..12], &[0xAB, 0xCD, 0xEF]); // flags (24-bit)
    }

    #[test]
    fn nested_child_patches_own_size() {
        let mut buf = Vec::new();
        {
            let mut parent = BoxWriter::open(&mut buf, *b"moov");
            {
                let mut child = parent.child(*b"mvhd");
                child.u32(0xDEADBEEF);
            }
            {
                let mut child2 = parent.child(*b"trak");
                child2.u16(0xFEED);
            }
        }
        // moov size = 8 (moov hdr) + 12 (mvhd=8+4) + 10 (trak=8+2) = 30
        assert_eq!(buf.len(), 30);
        assert_eq!(&buf[0..4], &[0, 0, 0, 30]);
        assert_eq!(&buf[4..8], b"moov");
        // mvhd
        assert_eq!(&buf[8..12], &[0, 0, 0, 12]);
        assert_eq!(&buf[12..16], b"mvhd");
        assert_eq!(&buf[16..20], &[0xDE, 0xAD, 0xBE, 0xEF]);
        // trak
        assert_eq!(&buf[20..24], &[0, 0, 0, 10]);
        assert_eq!(&buf[24..28], b"trak");
        assert_eq!(&buf[28..30], &[0xFE, 0xED]);
    }

    #[test]
    fn u24_big_endian() {
        let mut buf = Vec::new();
        {
            let mut b = BoxWriter::open(&mut buf, *b"test");
            b.u24(0x12_3456);
        }
        assert_eq!(&buf[8..11], &[0x12, 0x34, 0x56]);
    }

    #[test]
    fn patch_u32_overwrites_inline() {
        let mut buf = vec![0, 0, 0, 0, 1, 2, 3, 4];
        patch_u32(&mut buf, 0, 0xAABBCCDD);
        assert_eq!(&buf[0..4], &[0xAA, 0xBB, 0xCC, 0xDD]);
        // Remainder untouched.
        assert_eq!(&buf[4..], &[1, 2, 3, 4]);
    }

    #[test]
    fn cursor_pos_tracks_writes() {
        let mut buf = Vec::new();
        let start_before = buf.len();
        {
            let mut b = BoxWriter::open(&mut buf, *b"test");
            assert_eq!(b.box_start(), start_before);
            assert_eq!(b.cursor_pos(), start_before + 8); // header
            b.u32(0);
            assert_eq!(b.cursor_pos(), start_before + 12);
        }
    }
}
