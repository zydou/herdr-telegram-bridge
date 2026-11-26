//! The in-memory byte-cursor tail over session jsonl files.
//!
//! Cursors live in a `HashMap<PathBuf, u64>` and die with the process: a
//! restart re-attaches at EOF and never replays history. A file's first
//! sighting starts at its current size; later visits read from
//! `min(offset, size)` — a file replaced in place by something shorter must
//! not be read as a wall of "new" bytes — and consume only up to the last
//! complete newline, so a half-written line waits for the next sweep.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Tails {
    offsets: HashMap<PathBuf, u64>,
}

impl Tails {
    /// The new complete lines appended since the last call, joined with
    /// newlines. None when this is a first sighting (EOF start), the file is
    /// unreadable, or nothing complete has landed yet. The cursor advances
    /// whenever complete lines exist — even when every line turns out to be
    /// noise the renderer filters away, it must never be read twice.
    pub fn drain(&mut self, path: &Path) -> Option<String> {
        let size = fs::metadata(path).ok()?.len();
        let offset = match self.offsets.get(path) {
            None => {
                // first sighting: attach at EOF, never replay
                self.offsets.insert(path.to_path_buf(), size);
                return None;
            }
            Some(o) => {
                let clamped = (*o).min(size);
                // written back: a file truncated in place heals its cursor to
                // the new EOF, so bytes appended afterwards are actually read
                self.offsets.insert(path.to_path_buf(), clamped);
                clamped
            }
        };
        if offset == size {
            return None;
        }
        let mut f = fs::File::open(path).ok()?;
        f.seek(SeekFrom::Start(offset)).ok()?;
        let mut buf = Vec::with_capacity((size - offset) as usize);
        if f.read_to_end(&mut buf).ok()? == 0 {
            return None;
        }
        let cut = buf.iter().rposition(|&b| b == b'\n')?;
        self.offsets
            .insert(path.to_path_buf(), offset + cut as u64 + 1);
        let complete = &buf[..=cut];
        if complete == b"\n" {
            return Some(String::new()); // a lone empty line: consumed, nothing to render
        }
        Some(
            String::from_utf8_lossy(complete)
                .trim_end_matches('\n')
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sighting_starts_at_eof() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("s.jsonl");
        fs::write(&p, "history line\n").unwrap();
        let mut t = Tails::default();
        assert!(t.drain(&p).is_none());
        // and nothing replays afterwards
        assert!(t.drain(&p).is_none());
    }

    #[test]
    fn appended_complete_lines_arrive() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("s.jsonl");
        fs::write(&p, "a\n").unwrap();
        let mut t = Tails::default();
        assert!(t.drain(&p).is_none());
        fs::write(&p, "a\nb\nc\n").unwrap();
        assert_eq!(t.drain(&p).unwrap(), "b\nc");
        // fully consumed
        assert!(t.drain(&p).is_none());
    }

    #[test]
    fn half_line_waits_for_the_next_sweep() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("s.jsonl");
        fs::write(&p, "").unwrap();
        let mut t = Tails::default();
        assert!(t.drain(&p).is_none());
        fs::write(&p, "{\"half\": tru").unwrap();
        assert!(t.drain(&p).is_none());
        fs::write(&p, "{\"half\": true}\n").unwrap();
        assert_eq!(t.drain(&p).unwrap(), "{\"half\": true}");
    }

    #[test]
    fn truncated_file_does_not_flood() {
        // the file was replaced in place by something shorter
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("s.jsonl");
        fs::write(&p, "0123456789\n").unwrap();
        let mut t = Tails::default();
        assert!(t.drain(&p).is_none());
        fs::write(&p, "new\n").unwrap(); // smaller than the old offset
        assert!(t.drain(&p).is_none()); // min(offset, size) guards the jump back
        fs::write(&p, "new\nx\n").unwrap();
        assert_eq!(t.drain(&p).unwrap(), "x");
    }

    #[test]
    fn missing_file_is_none() {
        let mut t = Tails::default();
        assert!(t.drain(Path::new("/nowhere/s.jsonl")).is_none());
    }
}
