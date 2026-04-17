//! TUI client: connects to daemon's agent TCP port, raw terminal passthrough.
//!
//! Ctrl+B d to detach. Agent keeps running.

use crate::framing::{self, PROTOCOL_VERSION, TAG_DATA};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use std::io::{Read, Write};
use std::path::Path;

/// RAII guard for crossterm raw mode.
struct RawModeGuard;
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        terminal::disable_raw_mode().ok();
    }
}

/// Resolve the agent's TCP port, connect, enter raw mode, bridge terminal.
pub fn attach(home: &Path, name: &str) -> anyhow::Result<()> {
    let mut stream = crate::ipc::connect_agent(home, name)
        .map_err(|e| anyhow::anyhow!("Failed to connect to agent '{name}': {e}"))?;

    // Read protocol version byte from server
    let mut version_buf = [0u8; 1];
    stream
        .read_exact(&mut version_buf)
        .map_err(|e| anyhow::anyhow!("Failed to read protocol version: {e}"))?;
    if version_buf[0] != PROTOCOL_VERSION {
        anyhow::bail!(
            "Protocol version mismatch: server={} client={}",
            version_buf[0],
            PROTOCOL_VERSION
        );
    }

    terminal::enable_raw_mode()?;
    let _guard = RawModeGuard;

    let mut write_stream = stream.try_clone()?;
    let read_stream = stream;

    // Send initial terminal size
    let (cols, rows) = terminal::size().unwrap_or((120, 40));
    let _ = framing::write_resize(&mut write_stream, cols, rows);

    // Output thread: agent → terminal stdout
    std::thread::Builder::new()
        .name("tui_output".into())
        .spawn(move || {
            let mut stdout = std::io::stdout();
            let mut reader = read_stream;
            loop {
                match framing::read_tagged_frame(&mut reader) {
                    Ok((TAG_DATA, data)) => {
                        stdout.write_all(&data).ok();
                        stdout.flush().ok();
                    }
                    Ok(_) => {} // Ignore unknown tags
                    Err(_) => break,
                }
            }
            tracing::info!("connection closed");
        })?;

    // Input loop: crossterm events → agent
    let mut ctrl_b_pressed = false;
    loop {
        if !event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
            continue;
        }
        match event::read() {
            // Windows emits both Press and Release; ignore Release to keep
            // prefix state (ctrl_b_pressed) from toggling off immediately.
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Press => {}
            Ok(Event::Key(KeyEvent {
                code, modifiers, ..
            })) => {
                // Ctrl+B d combo for detach
                if ctrl_b_pressed {
                    ctrl_b_pressed = false;
                    if code == KeyCode::Char('d') && modifiers.is_empty() {
                        tracing::info!("detached");
                        break;
                    }
                    // Not 'd' — send Ctrl+B + current key
                    let mut bytes = vec![0x02];
                    bytes.extend(key_to_bytes(code, modifiers));
                    if framing::write_frame(&mut write_stream, &bytes).is_err() {
                        break;
                    }
                    continue;
                }
                if code == KeyCode::Char('b') && modifiers.contains(KeyModifiers::CONTROL) {
                    ctrl_b_pressed = true;
                    continue;
                }
                let bytes = key_to_bytes(code, modifiers);
                if !bytes.is_empty() && framing::write_frame(&mut write_stream, &bytes).is_err() {
                    break;
                }
            }
            Ok(Event::Paste(text)) => {
                if framing::write_frame(&mut write_stream, text.as_bytes()).is_err() {
                    break;
                }
            }
            Ok(Event::Resize(cols, rows)) => {
                if framing::write_resize(&mut write_stream, cols, rows).is_err() {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    // _guard dropped here — terminal restored
    println!();
    Ok(())
}

/// Convert crossterm KeyEvent to terminal bytes.
pub fn key_to_bytes(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    match code {
        KeyCode::Char(c) if ctrl => {
            vec![(c.to_ascii_lowercase() as u8)
                .wrapping_sub(b'a')
                .wrapping_add(1)]
        }
        KeyCode::Char(c) if alt => {
            let mut v = vec![0x1b];
            let mut b = [0u8; 4];
            v.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            v
        }
        KeyCode::Char(c) => {
            let mut b = [0u8; 4];
            c.encode_utf8(&mut b).as_bytes().to_vec()
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => vec![],
        },
        _ => vec![],
    }
}
