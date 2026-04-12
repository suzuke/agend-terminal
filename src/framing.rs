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
