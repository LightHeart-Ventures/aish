//! Mid-turn input line editor — the pure, testable core that lets an operator
//! **type at a prompt while a model turn is running** (thinking / mid tool-call),
//! not only at the idle rustyline prompt.
//!
//! ## Scope of this module
//! This module is deliberately **side-effect free**: it owns no tty, no termios,
//! no threads. It provides two reusable pieces the wiring layer composes:
//!
//! * [`KeyParser`] — folds a raw cbreak byte stream (the same bytes
//!   [`crate::keywatch`]'s reader already reads during a turn) into a sequence of
//!   semantic [`Key`]s. It assembles UTF-8 multi-byte characters and CSI escape
//!   sequences even when they are **fragmented across `read()` chunks**, carrying
//!   state between calls exactly like the existing `scan_csi_z` does for
//!   Shift-Tab — of which this parser is a strict superset (`ESC [ Z` ⇒
//!   [`Key::ShiftTab`]).
//! * [`LineBuf`] — a minimal, UTF-8-correct single-line editor (insert, cursor
//!   moves, backspace/delete, kill-to-start) with a render string for the footer.
//!
//! ## Why a separate module (vs. extending keywatch)
//! `keywatch::reader_loop` is a carefully-coordinated tty state machine
//! (cbreak↔cooked flips, foreground-pgrp handoff, confirm-prompt parking). Key
//! *interpretation* and *line editing* are pure data transforms with rich edge
//! cases (UTF-8 boundaries, CSI fragmentation, cursor arithmetic) that deserve
//! exhaustive unit tests without a real terminal. Splitting them keeps the reader
//! loop about tty ownership and this module about bytes→keys→text. The wiring
//! that connects them (render into the footer, queue a submitted line for the
//! next turn) is described in `docs/midturn-input.md`.
//!
//! ## Design semantic: type-ahead, not injection
//! A line submitted mid-turn is **queued** and run as the next command once the
//! current turn finishes — it does not mutate the in-flight turn. This matches
//! the existing `injected: Option<String>` path in the REPL and keeps the model
//! request stream well-formed. See the design doc for the drain point.

use unicode_width::UnicodeWidthStr;

/// A semantic key decoded from the raw byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    /// A printable character (already UTF-8 assembled).
    Char(char),
    /// Enter / Return (CR or LF) — submit the current line.
    Enter,
    /// Backspace (DEL 0x7f or BS 0x08) — delete the char before the cursor.
    Backspace,
    /// Forward-delete (CSI `3~`) — delete the char under the cursor.
    Delete,
    /// Shift-Tab / back-tab (CSI `Z`) — cycle the attach cursor (worker view).
    ShiftTab,
    /// Left arrow (CSI `D`).
    Left,
    /// Right arrow (CSI `C`).
    Right,
    /// Up arrow (CSI `A`).
    Up,
    /// Down arrow (CSI `B`).
    Down,
    /// Home (CSI `H`, CSI `1~`, or CSI `7~`).
    Home,
    /// End (CSI `F`, CSI `4~`, or CSI `8~`).
    End,
    /// Ctrl-U — kill from cursor to start of line.
    CtrlU,
    /// Ctrl-W — kill the word before the cursor.
    CtrlW,
    /// Ctrl-A — move to start of line.
    CtrlA,
    /// Ctrl-E — move to end of line.
    CtrlE,
}

/// Parser carry state so an escape sequence or UTF-8 character split across two
/// `read()` chunks still decodes correctly.
#[derive(Debug, Default)]
pub struct KeyParser {
    /// CSI accumulation state: `None` = ground; `Some(buf)` = mid escape, `buf`
    /// holds bytes seen after `ESC` (not including the ESC itself).
    esc: Option<Vec<u8>>,
    /// Pending UTF-8 continuation bytes for a multi-byte character in progress.
    utf8: Vec<u8>,
    /// How many more continuation bytes the in-progress UTF-8 char still needs.
    utf8_need: usize,
}

impl KeyParser {
    /// Create a fresh ground-state parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of bytes, appending decoded keys to `out`. Any partial
    /// escape / UTF-8 sequence at the end of `bytes` is retained as carry state
    /// and completed on a subsequent call. Bytes that don't form a recognised
    /// key (e.g. an unsupported CSI final) are silently dropped, matching the
    /// existing reader's "ignore what we don't handle" posture.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Key>) {
        for &b in bytes {
            self.feed_byte(b, out);
        }
    }

    /// Convenience: feed a chunk and return the freshly decoded keys.
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<Key> {
        let mut out = Vec::new();
        self.feed(bytes, &mut out);
        out
    }

    fn feed_byte(&mut self, b: u8, out: &mut Vec<Key>) {
        // In the middle of a CSI / ESC sequence?
        if let Some(buf) = self.esc.as_mut() {
            // A stray ESC re-arms the sequence (matches scan_csi_z semantics).
            if b == 0x1b {
                buf.clear();
                return;
            }
            buf.push(b);
            // First byte after ESC decides the shape.
            if buf.len() == 1 {
                match b {
                    b'[' | b'O' => return, // CSI / SS3 introducer, keep accumulating
                    _ => {
                        // ESC + single byte we don't model → drop, back to ground.
                        self.esc = None;
                        return;
                    }
                }
            }
            // We're in `ESC [` (or `ESC O`). Wait for a final byte in 0x40..=0x7e.
            if (0x40..=0x7e).contains(&b) {
                // buf = [ '[' , params..., final ]
                let seq = std::mem::take(buf);
                self.esc = None;
                if let Some(k) = decode_csi(&seq) {
                    out.push(k);
                }
            }
            return;
        }

        // Mid UTF-8 multi-byte char?
        if self.utf8_need > 0 {
            if b & 0b1100_0000 == 0b1000_0000 {
                self.utf8.push(b);
                self.utf8_need -= 1;
                if self.utf8_need == 0 {
                    if let Ok(s) = std::str::from_utf8(&self.utf8) {
                        if let Some(c) = s.chars().next() {
                            out.push(Key::Char(c));
                        }
                    }
                    self.utf8.clear();
                }
            } else {
                // Malformed continuation — abandon the partial char and reprocess
                // this byte from ground.
                self.utf8.clear();
                self.utf8_need = 0;
                self.feed_byte(b, out);
            }
            return;
        }

        match b {
            0x1b => self.esc = Some(Vec::new()), // ESC → start a sequence
            b'\r' | b'\n' => out.push(Key::Enter),
            0x7f | 0x08 => out.push(Key::Backspace),
            0x01 => out.push(Key::CtrlA),
            0x05 => out.push(Key::CtrlE),
            0x15 => out.push(Key::CtrlU),
            0x17 => out.push(Key::CtrlW),
            0x09 => out.push(Key::Char('\t')), // literal Tab (Shift-Tab is CSI Z)
            0x00..=0x1f => {} // other control bytes: ignore (ISIG handles ^C/^Z)
            b if b < 0x80 => out.push(Key::Char(b as char)),
            b => {
                // UTF-8 lead byte — set up continuation accounting.
                let need = if b & 0b1110_0000 == 0b1100_0000 {
                    1
                } else if b & 0b1111_0000 == 0b1110_0000 {
                    2
                } else if b & 0b1111_1000 == 0b1111_0000 {
                    3
                } else {
                    0 // invalid lead → drop
                };
                if need > 0 {
                    self.utf8.clear();
                    self.utf8.push(b);
                    self.utf8_need = need;
                }
            }
        }
    }
}

/// Decode a CSI body (the bytes after `ESC`, i.e. `[` … final) into a [`Key`].
/// Returns `None` for sequences we don't model.
fn decode_csi(seq: &[u8]) -> Option<Key> {
    // seq[0] is the introducer '[' or 'O'; last byte is the final.
    let final_b = *seq.last()?;
    let params = &seq[1..seq.len().saturating_sub(1)];
    match final_b {
        b'Z' => Some(Key::ShiftTab),
        b'A' => Some(Key::Up),
        b'B' => Some(Key::Down),
        b'C' => Some(Key::Right),
        b'D' => Some(Key::Left),
        b'H' => Some(Key::Home),
        b'F' => Some(Key::End),
        b'~' => match params {
            b"1" | b"7" => Some(Key::Home),
            b"3" => Some(Key::Delete),
            b"4" | b"8" => Some(Key::End),
            _ => None,
        },
        _ => None,
    }
}

/// A minimal, UTF-8-correct single-line editor for the mid-turn prompt. Cursor
/// is a **char** index in `0..=chars.len()`.
#[derive(Debug, Default, Clone)]
pub struct LineBuf {
    chars: Vec<char>,
    cursor: usize,
}

impl LineBuf {
    /// A fresh empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current text.
    pub fn as_string(&self) -> String {
        self.chars.iter().collect()
    }

    /// Cursor position as a char index.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Number of chars in the buffer.
    pub fn len(&self) -> usize {
        self.chars.len()
    }

    /// True when the buffer holds no chars.
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// Reset to empty.
    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    /// Take the current line, leaving the buffer empty. Returns `None` when the
    /// line is empty/whitespace-only (nothing to submit).
    pub fn take(&mut self) -> Option<String> {
        let s: String = self.chars.iter().collect();
        self.clear();
        if s.trim().is_empty() {
            None
        } else {
            Some(s)
        }
    }

    fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    fn kill_to_start(&mut self) {
        self.chars.drain(0..self.cursor);
        self.cursor = 0;
    }

    fn kill_word(&mut self) {
        // Skip trailing spaces, then the word, both to the left of the cursor.
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.chars.drain(i..self.cursor);
        self.cursor = i;
    }

    /// Fold a decoded [`Key`] into the buffer. Editing keys mutate state and
    /// return [`Action::None`]; the two "escape hatch" keys return an [`Action`]
    /// for the caller to act on: [`Action::Submit`] on Enter (with the line, if
    /// non-empty) and [`Action::CycleWorker`] on Shift-Tab.
    pub fn apply(&mut self, key: Key) -> Action {
        match key {
            Key::Char(c) => {
                self.insert(c);
                Action::None
            }
            Key::Backspace => {
                self.backspace();
                Action::None
            }
            Key::Delete => {
                self.delete();
                Action::None
            }
            Key::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                Action::None
            }
            Key::Right => {
                if self.cursor < self.chars.len() {
                    self.cursor += 1;
                }
                Action::None
            }
            Key::Home | Key::CtrlA => {
                self.cursor = 0;
                Action::None
            }
            Key::End | Key::CtrlE => {
                self.cursor = self.chars.len();
                Action::None
            }
            Key::CtrlU => {
                self.kill_to_start();
                Action::None
            }
            Key::CtrlW => {
                self.kill_word();
                Action::None
            }
            // Up/Down are reserved for future history recall; today they no-op
            // rather than corrupting the line.
            Key::Up | Key::Down => Action::None,
            Key::Enter => match self.take() {
                Some(line) => Action::Submit(line),
                None => Action::None,
            },
            Key::ShiftTab => Action::CycleWorker,
        }
    }
}

/// The caller-visible outcome of folding a key into a [`LineBuf`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing for the caller to do beyond re-rendering the footer.
    None,
    /// Enter was pressed on a non-empty line — queue it as the next command.
    Submit(String),
    /// Shift-Tab — cycle the attach cursor (mirror of the existing behaviour).
    CycleWorker,
}

/// Pure ANSI renderer for the mid-turn footer prompt: the escape-sequence math
/// that paints the prompt one blank line below the current output and erases it
/// again before the next streamed write. Kept pure (values in → `String` out)
/// so the fiddly cursor arithmetic is exhaustively unit-tested without a real
/// terminal.
///
/// ## Cursor contract (draw and erase are strict inverses)
/// [`draw`](Self::draw) is emitted with the terminal cursor at column 0 of a
/// fresh line `L0` — i.e. immediately after a streamed line's trailing newline.
/// It writes a blank gap line, then `prompt + text`, leaving the cursor at the
/// end of the prompt text. [`erase`](Self::erase) is emitted with the cursor
/// still at that end position; it clears every physical row the prompt occupies
/// **plus** the blank gap line and returns the cursor to column 0 of `L0` —
/// exactly where `draw` began — so the engine's next write lands as if the
/// footer never existed.
///
/// ## Concurrency note (why this is only the *math*)
/// Painting a footer that stays pinned below continuously-streamed turn output
/// requires that **every** engine write during a turn be serialized against the
/// reader thread's repaint through one shared lock (erase → write → redraw).
/// That serialization is the wiring layer's job (see `docs/design/midturn-input.md`);
/// this type deliberately owns no tty and no lock so the escape sequences can be
/// asserted byte-for-byte in tests.
pub struct FooterRender<'a> {
    /// The prompt sigil (e.g. `"» "`), rendered verbatim before the text.
    pub prompt: &'a str,
    /// The current line-editor contents ([`LineBuf::as_string`]).
    pub text: &'a str,
    /// Terminal width in columns, used to count wrapped physical rows. `0` means
    /// "unknown" → assume the prompt fits on a single row.
    pub cols: usize,
}

impl<'a> FooterRender<'a> {
    /// Physical terminal rows the prompt line occupies (always ≥ 1), accounting
    /// for soft-wrap at `cols`. Uses display width (so wide CJK/emoji count as 2).
    pub fn prompt_rows(&self) -> usize {
        let w = UnicodeWidthStr::width(self.prompt) + UnicodeWidthStr::width(self.text);
        if self.cols == 0 {
            return 1;
        }
        // A line of display-width `w` at `cols` columns spans `w / cols + 1`
        // physical rows (integer floor + 1); the +1 keeps draw/erase symmetric
        // even at an exact multiple, where the caret rests on the next row.
        w / self.cols + 1
    }

    /// ANSI to paint the footer: a blank gap line then `prompt + text`, leaving
    /// the cursor at the end of the text.
    pub fn draw(&self) -> String {
        format!("\n{}{}", self.prompt, self.text)
    }

    /// ANSI to remove a previously-[`draw`](Self::draw)n footer, returning the
    /// cursor to column 0 of the line where `draw` began. Clears the last prompt
    /// row, then walks up clearing the remaining prompt rows and the blank gap.
    pub fn erase(&self) -> String {
        let rows = self.prompt_rows();
        let mut s = String::from("\r\x1b[2K");
        for _ in 0..rows {
            s.push_str("\x1b[1A\x1b[2K");
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<Key> {
        KeyParser::new().decode(bytes)
    }

    #[test]
    fn plain_ascii_text() {
        assert_eq!(
            keys(b"hi"),
            vec![Key::Char('h'), Key::Char('i')]
        );
    }

    #[test]
    fn enter_cr_or_lf() {
        assert_eq!(keys(b"\r"), vec![Key::Enter]);
        assert_eq!(keys(b"\n"), vec![Key::Enter]);
    }

    #[test]
    fn backspace_both_codes() {
        assert_eq!(keys(b"\x7f"), vec![Key::Backspace]);
        assert_eq!(keys(b"\x08"), vec![Key::Backspace]);
    }

    #[test]
    fn shift_tab_is_csi_z() {
        assert_eq!(keys(b"\x1b[Z"), vec![Key::ShiftTab]);
    }

    #[test]
    fn arrows_home_end() {
        assert_eq!(keys(b"\x1b[A"), vec![Key::Up]);
        assert_eq!(keys(b"\x1b[B"), vec![Key::Down]);
        assert_eq!(keys(b"\x1b[C"), vec![Key::Right]);
        assert_eq!(keys(b"\x1b[D"), vec![Key::Left]);
        assert_eq!(keys(b"\x1b[H"), vec![Key::Home]);
        assert_eq!(keys(b"\x1b[F"), vec![Key::End]);
    }

    #[test]
    fn tilde_sequences() {
        assert_eq!(keys(b"\x1b[3~"), vec![Key::Delete]);
        assert_eq!(keys(b"\x1b[1~"), vec![Key::Home]);
        assert_eq!(keys(b"\x1b[4~"), vec![Key::End]);
    }

    #[test]
    fn literal_tab_distinct_from_shift_tab() {
        assert_eq!(keys(b"\t"), vec![Key::Char('\t')]);
    }

    #[test]
    fn ctrl_keys() {
        assert_eq!(keys(b"\x01"), vec![Key::CtrlA]);
        assert_eq!(keys(b"\x05"), vec![Key::CtrlE]);
        assert_eq!(keys(b"\x15"), vec![Key::CtrlU]);
        assert_eq!(keys(b"\x17"), vec![Key::CtrlW]);
    }

    #[test]
    fn csi_fragmented_across_reads() {
        let mut p = KeyParser::new();
        assert_eq!(p.decode(b"\x1b"), vec![]);
        assert_eq!(p.decode(b"["), vec![]);
        assert_eq!(p.decode(b"Z"), vec![Key::ShiftTab]);
    }

    #[test]
    fn utf8_multibyte_in_one_chunk() {
        // 'é' = C3 A9, '→' = E2 86 92
        assert_eq!(keys("é→".as_bytes()), vec![Key::Char('é'), Key::Char('→')]);
    }

    #[test]
    fn utf8_fragmented_across_reads() {
        let mut p = KeyParser::new();
        let bytes = "é".as_bytes(); // [0xC3, 0xA9]
        assert_eq!(p.decode(&bytes[..1]), vec![]);
        assert_eq!(p.decode(&bytes[1..]), vec![Key::Char('é')]);
    }

    #[test]
    fn double_esc_rearms() {
        // Mirrors keywatch::scan_csi_z double_esc_rearms.
        assert_eq!(keys(b"\x1b\x1b[Z"), vec![Key::ShiftTab]);
    }

    #[test]
    fn mixed_text_edit_and_shift_tab_stream() {
        let out = keys(b"ab\x1b[Zc\r");
        assert_eq!(
            out,
            vec![
                Key::Char('a'),
                Key::Char('b'),
                Key::ShiftTab,
                Key::Char('c'),
                Key::Enter,
            ]
        );
    }

    // ---- LineBuf ----

    fn typed(s: &str) -> LineBuf {
        let mut b = LineBuf::new();
        for k in KeyParser::new().decode(s.as_bytes()) {
            b.apply(k);
        }
        b
    }

    #[test]
    fn insert_and_render() {
        let b = typed("hello");
        assert_eq!(b.as_string(), "hello");
        assert_eq!(b.cursor(), 5);
    }

    #[test]
    fn backspace_edits_at_cursor() {
        let mut b = typed("helo");
        // move left once (before the 'o'), insert 'l' → "hello"
        b.apply(Key::Left);
        b.apply(Key::Char('l'));
        assert_eq!(b.as_string(), "hello");
    }

    #[test]
    fn home_end_and_insert() {
        let mut b = typed("world");
        b.apply(Key::Home);
        b.apply(Key::Char('¡'));
        assert_eq!(b.as_string(), "¡world");
        b.apply(Key::End);
        b.apply(Key::Char('!'));
        assert_eq!(b.as_string(), "¡world!");
    }

    #[test]
    fn ctrl_u_kills_to_start() {
        let mut b = typed("keep this");
        // cursor at end; move left 4 (before "this"), then CtrlU kills "keep "
        for _ in 0..4 {
            b.apply(Key::Left);
        }
        b.apply(Key::CtrlU);
        assert_eq!(b.as_string(), "this");
    }

    #[test]
    fn ctrl_w_kills_word() {
        let mut b = typed("foo bar");
        b.apply(Key::CtrlW);
        assert_eq!(b.as_string(), "foo ");
    }

    #[test]
    fn delete_removes_under_cursor() {
        let mut b = typed("abc");
        b.apply(Key::Home);
        b.apply(Key::Delete);
        assert_eq!(b.as_string(), "bc");
    }

    #[test]
    fn enter_submits_and_clears() {
        let mut b = typed("run it");
        let action = b.apply(Key::Enter);
        assert_eq!(action, Action::Submit("run it".to_string()));
        assert!(b.is_empty());
    }

    #[test]
    fn enter_on_empty_is_noop() {
        let mut b = LineBuf::new();
        assert_eq!(b.apply(Key::Enter), Action::None);
        // whitespace-only also no-ops
        b.apply(Key::Char(' '));
        b.apply(Key::Char('\t'));
        assert_eq!(b.apply(Key::Enter), Action::None);
    }

    #[test]
    fn shift_tab_yields_cycle_action() {
        let mut b = typed("half typed");
        assert_eq!(b.apply(Key::ShiftTab), Action::CycleWorker);
        // Shift-Tab must NOT discard the in-progress line.
        assert_eq!(b.as_string(), "half typed");
    }

    #[test]
    fn cursor_bounds_never_panic() {
        let mut b = LineBuf::new();
        b.apply(Key::Left); // underflow guard
        b.apply(Key::Right); // overflow guard
        b.apply(Key::Backspace); // empty guard
        b.apply(Key::Delete); // empty guard
        assert!(b.is_empty());
        assert_eq!(b.cursor(), 0);
    }

    // ---- FooterRender ----

    #[test]
    fn footer_draw_is_blank_gap_then_prompt() {
        let f = FooterRender { prompt: "» ", text: "hi", cols: 80 };
        assert_eq!(f.draw(), "\n» hi");
    }

    #[test]
    fn footer_single_row_erase_is_inverse() {
        let f = FooterRender { prompt: "» ", text: "hi", cols: 80 };
        assert_eq!(f.prompt_rows(), 1);
        // clear prompt row, then up-and-clear the blank gap line.
        assert_eq!(f.erase(), "\r\x1b[2K\x1b[1A\x1b[2K");
    }

    #[test]
    fn footer_empty_text_still_one_row() {
        let f = FooterRender { prompt: "» ", text: "", cols: 80 };
        assert_eq!(f.prompt_rows(), 1);
        assert_eq!(f.draw(), "\n» ");
    }

    #[test]
    fn footer_unknown_width_assumes_single_row() {
        let f = FooterRender { prompt: "» ", text: "a very long line that would wrap", cols: 0 };
        assert_eq!(f.prompt_rows(), 1);
    }

    #[test]
    fn footer_wraps_to_multiple_rows() {
        // prompt width 2 + text width 10 = 12 cols; at width 6 → 12/6 + 1 = 3 rows.
        let f = FooterRender { prompt: "» ", text: "0123456789", cols: 6 };
        assert_eq!(f.prompt_rows(), 3);
        // erase clears the last prompt row then walks up over 3 more lines
        // (2 remaining prompt rows + the blank gap).
        assert_eq!(f.erase(), "\r\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2K");
    }

    #[test]
    fn footer_wide_chars_count_as_two_columns() {
        // Two double-width chars (width 4) + prompt width 2 = 6 at cols 6 → 2 rows.
        let f = FooterRender { prompt: "» ", text: "中文", cols: 6 };
        assert_eq!(f.prompt_rows(), 2);
    }
}
