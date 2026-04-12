//! Tag-based framing protocol for TUI socket communication.
//!
//! Format: [u8 tag][u32 BE length][bytes]
//! Tag 0 = PTY data, Tag 1 = resize (4 bytes: cols_hi, cols_lo, rows_hi, rows_lo)

use std::io::{Read, Write};

pub const TAG_DATA: u8 = 0;
pub const TAG_RESIZE: u8 = 1;
/// Protocol version for TUI socket handshake.
pub const PROTOCOL_VERSION: u8 = 1;
/// Maximum frame size. Override via AGEND_FRAME_LIMIT env var (bytes).
pub const DEFAULT_FRAME_LIMIT: usize = 1_000_000;

fn frame_limit() -> usize {
    std::env::var("AGEND_FRAME_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_FRAME_LIMIT)
}

pub fn write_frame(w: &mut impl Write, data: &[u8]) -> std::io::Result<()> {
    if data.len() > frame_limit() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    w.write_all(&[TAG_DATA])?;
    w.write_all(&(data.len() as u32).to_be_bytes())?;
    w.write_all(data)?;
    w.flush()
}

pub fn write_tagged(w: &mut impl Write, tag: u8, data: &[u8]) -> std::io::Result<()> {
    w.write_all(&[tag])?;
    w.write_all(&(data.len() as u32).to_be_bytes())?;
    w.write_all(data)?;
    w.flush()
}

pub fn read_tagged_frame(r: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)?;
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > frame_limit() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok((tag[0], buf))
}

pub fn write_resize(w: &mut impl Write, cols: u16, rows: u16) -> std::io::Result<()> {
    let mut data = [0u8; 4];
    data[0..2].copy_from_slice(&cols.to_be_bytes());
    data[2..4].copy_from_slice(&rows.to_be_bytes());
    write_tagged(w, TAG_RESIZE, &data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn write_frame_read_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello world").expect("write");

        let mut cursor = Cursor::new(buf);
        let (tag, data) = read_tagged_frame(&mut cursor).expect("read");
        assert_eq!(tag, TAG_DATA);
        assert_eq!(data, b"hello world");
    }

    #[test]
    fn frame_with_tag_data() {
        let mut buf = Vec::new();
        write_tagged(&mut buf, TAG_DATA, b"payload").expect("write");

        let mut cursor = Cursor::new(buf);
        let (tag, data) = read_tagged_frame(&mut cursor).expect("read");
        assert_eq!(tag, TAG_DATA);
        assert_eq!(data, b"payload");
    }

    #[test]
    fn frame_with_tag_resize() {
        let mut buf = Vec::new();
        write_resize(&mut buf, 120, 40).expect("write");

        let mut cursor = Cursor::new(buf);
        let (tag, data) = read_tagged_frame(&mut cursor).expect("read");
        assert_eq!(tag, TAG_RESIZE);
        assert_eq!(data.len(), 4);
        let cols = u16::from_be_bytes([data[0], data[1]]);
        let rows = u16::from_be_bytes([data[2], data[3]]);
        assert_eq!(cols, 120);
        assert_eq!(rows, 40);
    }

    #[test]
    fn write_tagged_arbitrary_tag() {
        let mut buf = Vec::new();
        write_tagged(&mut buf, 42, b"custom").expect("write");

        let mut cursor = Cursor::new(buf);
        let (tag, data) = read_tagged_frame(&mut cursor).expect("read");
        assert_eq!(tag, 42);
        assert_eq!(data, b"custom");
    }

    #[test]
    fn empty_payload() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"").expect("write");

        let mut cursor = Cursor::new(buf);
        let (tag, data) = read_tagged_frame(&mut cursor).expect("read");
        assert_eq!(tag, TAG_DATA);
        assert!(data.is_empty());
    }

    #[test]
    fn multiple_frames_sequential() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"first").expect("write");
        write_frame(&mut buf, b"second").expect("write");
        write_tagged(&mut buf, TAG_RESIZE, &[0, 80, 0, 24]).expect("write");

        let mut cursor = Cursor::new(buf);
        let (t1, d1) = read_tagged_frame(&mut cursor).expect("read 1");
        let (t2, d2) = read_tagged_frame(&mut cursor).expect("read 2");
        let (t3, d3) = read_tagged_frame(&mut cursor).expect("read 3");

        assert_eq!(t1, TAG_DATA);
        assert_eq!(d1, b"first");
        assert_eq!(t2, TAG_DATA);
        assert_eq!(d2, b"second");
        assert_eq!(t3, TAG_RESIZE);
        assert_eq!(d3, vec![0, 80, 0, 24]);
    }

    #[test]
    fn read_truncated_frame_errors() {
        // Only write the tag byte — no length
        let buf = vec![TAG_DATA];
        let mut cursor = Cursor::new(buf);
        assert!(read_tagged_frame(&mut cursor).is_err());
    }

    #[test]
    fn read_empty_stream_errors() {
        let buf: Vec<u8> = vec![];
        let mut cursor = Cursor::new(buf);
        assert!(read_tagged_frame(&mut cursor).is_err());
    }

    #[test]
    fn frame_too_large_errors() {
        // Craft a frame header claiming a huge payload
        let mut buf = Vec::new();
        buf.push(TAG_DATA);
        let huge_len: u32 = (DEFAULT_FRAME_LIMIT as u32) + 1;
        buf.extend_from_slice(&huge_len.to_be_bytes());
        // Don't even write the payload — should fail on length check

        let mut cursor = Cursor::new(buf);
        let result = read_tagged_frame(&mut cursor);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn write_frame_format_is_tag_len_data() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"AB").expect("write");

        // [TAG_DATA=0] [len=2 as u32 BE] [0x41, 0x42]
        assert_eq!(buf.len(), 1 + 4 + 2);
        assert_eq!(buf[0], TAG_DATA);
        assert_eq!(&buf[1..5], &[0, 0, 0, 2]);
        assert_eq!(&buf[5..], b"AB");
    }

    #[test]
    fn resize_dimensions_max_values() {
        let mut buf = Vec::new();
        write_resize(&mut buf, u16::MAX, u16::MAX).expect("write");

        let mut cursor = Cursor::new(buf);
        let (tag, data) = read_tagged_frame(&mut cursor).expect("read");
        assert_eq!(tag, TAG_RESIZE);
        let cols = u16::from_be_bytes([data[0], data[1]]);
        let rows = u16::from_be_bytes([data[2], data[3]]);
        assert_eq!(cols, u16::MAX);
        assert_eq!(rows, u16::MAX);
    }

    #[test]
    fn constants_correct() {
        assert_eq!(TAG_DATA, 0);
        assert_eq!(TAG_RESIZE, 1);
        assert_eq!(PROTOCOL_VERSION, 1);
        assert!(DEFAULT_FRAME_LIMIT > 0);
    }
}
