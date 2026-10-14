//! QR for the sign-in and share windows: text (a join link, anything) to a
//! module matrix, and the matrix to text for agents and terminals. Drawing
//! stays with the window (pixels) or the log (half-blocks).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

/// A QR module matrix: row-major, `true` is a dark module, no quiet zone
/// (the drawing side adds it, `quiet` modules wide by convention).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Matrix {
    pub width: usize,
    cells: Vec<bool>,
}

impl Matrix {
    /// Whether the module at `(row, col)` is dark. Out-of-range reads are
    /// light, never a panic: drawings clip.
    pub fn dark(&self, row: usize, col: usize) -> bool {
        if row >= self.width || col >= self.width {
            return false;
        }
        self.cells[row * self.width + col]
    }

    /// Every row, left to right.
    pub fn rows(&self) -> impl Iterator<Item = &[bool]> {
        self.cells.chunks(self.width)
    }

    /// The matrix as text: `██` per dark module, two spaces per light one,
    /// `quiet` modules of margin, one text row per module row (a log line
    /// and a chat message both read it).
    pub fn to_text(&self, quiet: usize) -> String {
        let mut out = String::with_capacity((self.width + 2 * quiet) * (self.width + 2 * quiet) * 2);
        let blank = "  ".repeat(self.width + 2 * quiet);
        for _ in 0..quiet {
            out.push_str(&blank);
            out.push('\n');
        }
        for row in 0..self.width {
            for _ in 0..quiet {
                out.push_str("  ");
            }
            for col in 0..self.width {
                out.push_str(if self.dark(row, col) { "██" } else { "  " });
            }
            for _ in 0..quiet {
                out.push_str("  ");
            }
            out.push('\n');
        }
        for _ in 0..quiet {
            out.push_str(&blank);
            out.push('\n');
        }
        out
    }
}

/// Encodes `text` (byte mode, error correction M — the level join links
/// use) as a module matrix.
pub fn matrix(text: &str) -> Result<Matrix, String> {
    let code = qrcode::QrCode::new(text.as_bytes()).map_err(|e| format!("qr: {e}"))?;
    let width = code.width();
    let cells = code.to_colors().into_iter().map(|c| matches!(c, qrcode::Color::Dark)).collect::<Vec<bool>>();
    if cells.len() != width * width {
        return Err("qr: the encoder returned an unexpected module count".into());
    }
    Ok(Matrix { width, cells })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three finder patterns (7×7 squares with a 3×3 core) sit in the
    /// top-left, top-right and bottom-left corners of every QR symbol.
    fn has_finder(m: &Matrix, top: usize, left: usize) -> bool {
        if !m.dark(top, left) {
            return false;
        }
        for i in 0..7 {
            if !m.dark(top + i, left) || !m.dark(top, left + i) || !m.dark(top + 6, left + i) || !m.dark(top + i, left + 6) {
                return false;
            }
        }
        for i in 2..5 {
            for j in 2..5 {
                if !m.dark(top + i, left + j) {
                    return false;
                }
            }
        }
        !m.dark(top + 1, left + 1)
    }

    #[test]
    fn a_join_link_encodes_to_a_finder_pattern_symbol() {
        let link = "https://realm.macula.io/join/01a1247f-9c9b-7364-9155-d9514a958baf";
        let m = matrix(link).expect("encode");
        assert!(m.width >= 21 && m.width <= 177 && (m.width - 17).is_multiple_of(4), "a QR symbol width, got {}", m.width);
        assert!(has_finder(&m, 0, 0), "top-left finder");
        assert!(has_finder(&m, 0, m.width - 7), "top-right finder");
        assert!(has_finder(&m, m.width - 7, 0), "bottom-left finder");
        assert!(m.dark(6, 8) && m.dark(8, 6), "the timing patterns run between the finders");
    }

    /// A real decoder reads the link back out of the drawn symbol: the
    /// matrices this helper produces are scannable, not just shaped.
    #[test]
    fn a_decoder_reads_the_link_back() {
        let link = "https://realm.macula.io/join/01a1247f-9c9b-7364-9155-d9514a958baf";
        let m = matrix(link).expect("encode");
        let scale = 8usize;
        let quiet = 4usize;
        let side = (m.width + 2 * quiet) * scale;
        let mut img = rqrr::PreparedImage::prepare_from_greyscale(side, side, |x, y| {
            let (gx, gy) = (x / scale, y / scale);
            if gx < quiet || gy < quiet || gx >= quiet + m.width || gy >= quiet + m.width {
                return 255;
            }
            if m.dark(gy - quiet, gx - quiet) { 0u8 } else { 255u8 }
        });
        let grids = img.detect_grids();
        assert!(!grids.is_empty(), "a QR grid is found in the drawing");
        let decoded = grids[0].decode().expect("the grid decodes");
        assert_eq!(decoded.1, link, "the decoded text is exactly the link");
    }

    #[test]
    fn the_same_text_always_encodes_to_the_same_matrix() {
        let a = matrix("https://example.org/join/x").expect("encode");
        let b = matrix("https://example.org/join/x").expect("encode");
        assert_eq!(a, b, "deterministic");
        let c = matrix("https://example.org/join/y").expect("encode");
        assert_ne!(a, c, "different text, different modules");
    }

    #[test]
    fn the_text_form_has_a_quiet_zone_and_two_cells_per_module() {
        let m = matrix("https://example.org/join/x").expect("encode");
        let text = m.to_text(2);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), m.width + 4, "two quiet rows top and bottom");
        for line in &lines {
            let chars = line.chars().count();
            assert_eq!(chars, (m.width + 4) * 2, "two spaces per module plus quiet zone");
        }
        // The corner finder shows as block pairs in the text form: the
        // first matrix row is the finder's top edge, seven dark modules.
        let expected = format!("{}{}", "  ".repeat(2), "██".repeat(7));
        assert!(lines[2].starts_with(&expected), "quiet zone then the finder's top edge: {:?}", &lines[2][..20.min(lines[2].len())]);
    }
}
