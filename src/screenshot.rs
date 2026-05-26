//! POC #1257: TUI screenshot → SVG.
//!
//! Renders the TUI state into a TestBackend buffer, then converts
//! the cell grid to an SVG document with monospace text + colors.

use ratatui::backend::TestBackend;
use ratatui::style::Color;

/// Cell dimensions in the SVG (pixels).
const CELL_W: u16 = 8;
const CELL_H: u16 = 16;
const FONT_SIZE: u16 = 14;

/// Render a TestBackend buffer to an SVG string.
pub fn buffer_to_svg(backend: &TestBackend) -> String {
    let buf = backend.buffer().clone();
    let width = buf.area.width;
    let height = buf.area.height;
    let svg_w = width as u32 * CELL_W as u32;
    let svg_h = height as u32 * CELL_H as u32;

    let mut svg = String::with_capacity(svg_w as usize * svg_h as usize / 2);
    svg.push_str(&format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{svg_w}" height="{svg_h}" style="background:#1e1e2e">"#
    ));
    svg.push_str(&format!(
        r#"<style>text{{font-family:monospace;font-size:{FONT_SIZE}px;dominant-baseline:hanging}}</style>"#
    ));

    for y in 0..height {
        for x in 0..width {
            let cell = &buf[(x, y)];
            let symbol = cell.symbol();
            if symbol == " " || symbol.is_empty() {
                continue;
            }
            let fg = color_to_hex(cell.fg);
            let px = x as u32 * CELL_W as u32;
            let py = y as u32 * CELL_H as u32 + 2;
            let escaped = xml_escape(symbol);
            svg.push_str(&format!(
                r#"<text x="{px}" y="{py}" fill="{fg}">{escaped}</text>"#
            ));
        }
    }

    svg.push_str("</svg>");
    svg
}

/// Convert ratatui Color to hex. Falls back to #cdd6f4 (catppuccin text).
fn color_to_hex(c: Color) -> &'static str {
    match c {
        Color::Black => "#45475a",
        Color::Red => "#f38ba8",
        Color::Green => "#a6e3a1",
        Color::Yellow => "#f9e2af",
        Color::Blue => "#89b4fa",
        Color::Magenta => "#cba6f7",
        Color::Cyan => "#94e2d5",
        Color::White | Color::Reset => "#cdd6f4",
        Color::Gray => "#6c7086",
        Color::DarkGray => "#585b70",
        Color::LightRed => "#f38ba8",
        Color::LightGreen => "#a6e3a1",
        Color::LightYellow => "#f9e2af",
        Color::LightBlue => "#89b4fa",
        Color::LightMagenta => "#cba6f7",
        Color::LightCyan => "#94e2d5",
        _ => "#cdd6f4",
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// POC test: render a simple widget to TestBackend, convert to SVG.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use ratatui::widgets::{Block, Borders, Paragraph};
    use ratatui::Terminal;

    #[test]
    fn poc_buffer_to_svg_produces_valid_svg() {
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let block = Block::default().borders(Borders::ALL).title(" Task Board ");
                let paragraph = Paragraph::new("Hello from POC!").block(block);
                frame.render_widget(paragraph, frame.area());
            })
            .unwrap();
        let svg = buffer_to_svg(terminal.backend());
        assert!(svg.starts_with("<svg"), "must start with <svg");
        assert!(svg.ends_with("</svg>"), "must end with </svg>");
        assert!(
            svg.len() > 200,
            "SVG must have content, got {} bytes",
            svg.len()
        );
        // Verify text elements are present.
        assert!(svg.contains("<text"), "SVG must contain text elements");
        std::fs::write("/tmp/tui_screenshot_poc.svg", &svg).unwrap();
        eprintln!(
            "Written to /tmp/tui_screenshot_poc.svg ({} bytes)",
            svg.len()
        );
    }
}
