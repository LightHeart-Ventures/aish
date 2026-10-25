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
    /// A whole **bracketed paste** (`ESC [200~` … `ESC [201~`), already
    /// assembled and newline-normalised. Delivered as ONE key so a multi-line
    /// paste lands as one edit at the cursor instead of N phantom Enters: the
    /// operator still owns submission (`paste → Enter`, `input → paste → Enter`,
    /// `paste → input → Enter`, … are all valid).
    Paste(String),
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
    /// Bracketed-paste accumulation: `None` = not pasting; `Some(buf)` = inside
    /// `ESC [200~` … `ESC [201~`, `buf` holds the raw payload seen so far.
    /// Everything (newlines included) is buffered verbatim until the end marker.
    paste: Option<Vec<u8>>,
}

/// The bracketed-paste terminator emitted by the terminal after the payload.
const PASTE_END: &[u8] = b"\x1b[201~";

/// Normalise a raw pasted payload into the text we insert into the line buffer:
/// CRLF/CR → LF (so a Windows-clipboard paste doesn't carry stray `\r` into the
/// command) and a trailing newline trimmed (copying a whole line almost always
/// includes it, and it must NOT read as "submit"). Returns `None` when nothing
/// printable survives.
fn normalize_paste(raw: &str) -> Option<String> {
    let text = raw.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim_end_matches('\n').to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// Heuristic fallback for terminals that do NOT speak bracketed paste: a single
/// `read()` chunk carrying an **interior** newline cannot have been typed (a
/// human's Enter ends the chunk), so treat the whole chunk as one paste.
/// Conservative on purpose — chunks containing ESC are left to the key parser so
/// no escape sequence is ever swallowed.
fn looks_like_paste(bytes: &[u8]) -> bool {
    if bytes.len() < 2 || bytes.contains(&0x1b) {
        return false;
    }
    // An interior newline = a newline with at least one byte after it.
    bytes[..bytes.len() - 1]
        .iter()
        .any(|&b| b == b'\n' || b == b'\r')
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
        // Terminals without bracketed paste give us no markers at all; a chunk
        // with an interior newline is a paste by construction (see
        // `looks_like_paste`). Only consider it from a clean ground state so we
        // never cut a carried-over escape / UTF-8 / paste sequence in half.
        if self.is_ground() && looks_like_paste(bytes) {
            if let Some(text) = normalize_paste(&String::from_utf8_lossy(bytes)) {
                out.push(Key::Paste(text));
            }
            return;
        }
        for &b in bytes {
            self.feed_byte(b, out);
        }
    }

    /// True when no escape / UTF-8 / paste sequence is mid-flight.
    fn is_ground(&self) -> bool {
        self.esc.is_none() && self.utf8_need == 0 && self.paste.is_none()
    }

    /// Accumulate one payload byte of an in-flight bracketed paste, emitting a
    /// single [`Key::Paste`] once the `ESC [201~` terminator lands. The payload
    /// is buffered verbatim — a newline inside a paste is text, never a submit.
    fn feed_paste_byte(&mut self, b: u8, out: &mut Vec<Key>) {
        let Some(buf) = self.paste.as_mut() else {
            return;
        };
        buf.push(b);
        if !buf.ends_with(PASTE_END) {
            return;
        }
        let mut raw = std::mem::take(buf);
        raw.truncate(raw.len() - PASTE_END.len());
        self.paste = None;
        if let Some(text) = normalize_paste(&String::from_utf8_lossy(&raw)) {
            out.push(Key::Paste(text));
        }
    }

    /// Convenience: feed a chunk and return the freshly decoded keys.
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<Key> {
        let mut out = Vec::new();
        self.feed(bytes, &mut out);
        out
    }

    fn feed_byte(&mut self, b: u8, out: &mut Vec<Key>) {
        // Inside a bracketed paste every byte is payload — newlines, control
        // chars, the lot — until the `ESC [201~` terminator. This is what keeps a
        // 12-line paste from arriving as 12 submitted lines.
        if self.paste.is_some() {
            self.feed_paste_byte(b, out);
            return;
        }

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
                // Paste brackets are modes, not keys: `[200~` opens the payload
                // buffer, a stray `[201~` in ground state is noise.
                if seq.as_slice() == b"[200~" {
                    self.paste = Some(Vec::new());
                    return;
                }
                if seq.as_slice() == b"[201~" {
                    return;
                }
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
                    if let Ok(s) = std::str::from_utf8(&self.utf8)
                        && let Some(c) = s.chars().next()
                    {
                        out.push(Key::Char(c));
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
            0x00..=0x1f => {}                  // other control bytes: ignore (ISIG handles ^C/^Z)
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
///
/// Also owns mid-turn **history recall**: seeded with the REPL's history
/// (oldest → newest) at turn start, Up/Down walk it exactly like the
/// at-the-prompt editor does. `nav` is the cursor into that list (`None` ==
/// editing a live line, not a recalled one) and `draft` stashes the in-progress
/// text on the first Up so walking back down to the newest entry restores it
/// instead of eating it.
#[derive(Debug, Default, Clone)]
pub struct LineBuf {
    chars: Vec<char>,
    cursor: usize,
    /// History entries, oldest first (index 0) → newest last.
    hist: Vec<String>,
    /// Index into `hist` of the entry currently displayed, or `None` when the
    /// buffer holds the operator's own in-progress line.
    nav: Option<usize>,
    /// The in-progress line stashed when history recall started.
    draft: String,
}

impl LineBuf {
    /// A fresh empty buffer with no history (Up/Down are inert).
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh empty buffer seeded with `hist` (oldest first) for Up/Down
    /// recall. Consecutive duplicates are collapsed so repeated commands don't
    /// require repeated Ups to walk past.
    pub fn with_history(hist: Vec<String>) -> Self {
        let mut dedup: Vec<String> = Vec::with_capacity(hist.len());
        for entry in hist {
            if entry.trim().is_empty() {
                continue;
            }
            if dedup.last().map(String::as_str) == Some(entry.as_str()) {
                continue;
            }
            dedup.push(entry);
        }
        Self {
            hist: dedup,
            ..Self::default()
        }
    }

    /// Replace the whole line, parking the cursor at its end.
    fn set_line(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
    }

    /// Walk one entry back (older) through history. Returns `true` when the
    /// buffer changed, so the caller knows to repaint.
    fn hist_prev(&mut self) -> bool {
        if self.hist.is_empty() {
            return false;
        }
        let next = match self.nav {
            // First Up of this recall: stash the live line, show the newest.
            None => {
                self.draft = self.as_string();
                self.hist.len() - 1
            }
            // Already at the oldest entry — stay put (don't wrap).
            Some(0) => return false,
            Some(i) => i - 1,
        };
        self.nav = Some(next);
        let text = self.hist[next].clone();
        self.set_line(&text);
        true
    }

    /// Walk one entry forward (newer); past the newest entry the stashed draft
    /// comes back. Returns `true` when the buffer changed.
    fn hist_next(&mut self) -> bool {
        let Some(i) = self.nav else {
            return false;
        };
        if i + 1 < self.hist.len() {
            self.nav = Some(i + 1);
            let text = self.hist[i + 1].clone();
            self.set_line(&text);
        } else {
            // Walked off the newest entry → back to the operator's own line.
            self.nav = None;
            let draft = std::mem::take(&mut self.draft);
            self.set_line(&draft);
        }
        true
    }

    /// The current text.
    pub fn as_string(&self) -> String {
        self.chars.iter().collect()
    }

    /// Cursor position as a char index.
    #[allow(dead_code)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Number of chars in the buffer.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.chars.len()
    }

    /// True when the buffer holds no chars.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// Reset to empty (history itself is kept; only the recall cursor and the
    /// stashed draft are dropped).
    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
        self.nav = None;
        self.draft.clear();
    }

    /// Take the current line, leaving the buffer empty. Returns `None` when the
    /// line is empty/whitespace-only (nothing to submit).
    pub fn take(&mut self) -> Option<String> {
        let s: String = self.chars.iter().collect();
        self.clear();
        if s.trim().is_empty() { None } else { Some(s) }
    }

    /// Editing a recalled entry makes the line the operator's own again, so a
    /// later Down restores their draft instead of overwriting the edit with the
    /// next history entry.
    fn detach_from_history(&mut self) {
        self.nav = None;
    }

    fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
        self.detach_from_history();
    }

    /// Insert a whole string at the cursor as ONE edit — the paste path. Keeps
    /// newlines as literal chars so the block stays a single pending line that
    /// the operator submits (or edits further) with Enter.
    fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
        }
        self.detach_from_history();
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
            self.detach_from_history();
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
            self.detach_from_history();
        }
    }

    fn kill_to_start(&mut self) {
        self.chars.drain(0..self.cursor);
        self.cursor = 0;
        self.detach_from_history();
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
        self.detach_from_history();
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
            // A paste is ONE edit, never a sequence of submits: the whole
            // payload (newlines included) lands in the buffer and waits for the
            // operator's Enter. This is the GH bug — pasted lines were each
            // queued as separate input.
            Key::Paste(text) => {
                self.insert_str(&text);
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
            // Up/Down walk the seeded history (GH: up-arrow was inert while a
            // turn was in flight). Both are no-ops when there's nothing to
            // recall, so they never corrupt the line.
            Key::Up => {
                self.hist_prev();
                Action::None
            }
            Key::Down => {
                self.hist_next();
                Action::None
            }
            Key::Enter => match self.take() {
                Some(line) => {
                    // Make the just-submitted line recallable for the rest of
                    // the turn, mirroring the at-the-prompt editor.
                    if self.hist.last() != Some(&line) {
                        self.hist.push(line.clone());
                    }
                    Action::Submit(line)
                }
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
#[allow(dead_code)]
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub fn draw(&self) -> String {
        format!("\n{}{}", self.prompt, self.text)
    }

    /// ANSI to remove a previously-[`draw`](Self::draw)n footer, returning the
    /// cursor to column 0 of the line where `draw` began. Clears the last prompt
    /// row, then walks up clearing the remaining prompt rows and the blank gap.
    #[allow(dead_code)]
    pub fn erase(&self) -> String {
        let rows = self.prompt_rows();
        let mut s = String::from("\r\x1b[2K");
        for _ in 0..rows {
            s.push_str("\x1b[1A\x1b[2K");
        }
        s
    }
}

/// Colon commands that are safe — and USEFUL — to execute the instant they are
/// submitted mid-turn, instead of being queued until the turn ends.
///
/// Two properties qualify a command for this list:
///
/// 1. **It does not touch the model stream.** Queuing prose is deliberate (a
///    line injected into an in-flight request would malform it — see
///    `docs/design/midturn-input.md`). These commands never reach the backend,
///    so that reason doesn't apply.
/// 2. **It needs nothing but `Arc` handles already cloned off `Session`.** The
///    in-flight turn holds `&mut Session`, so anything that reads or mutates
///    session state (`:model`, `:clear`, `:restart`, …) CANNOT run concurrently
///    and must stay queued. The commands below operate purely on shared handles
///    (`worker_jobs`, `coordinator_store`, `attached`, …), captured up front in
///    an [`crate::repl::OpsCtx`] snapshot.
///
/// Queuing these was the reported bug: `:dispatch` typed while aish was thinking
/// didn't launch until the turn it was meant to run ALONGSIDE had already
/// finished, defeating the entire point of dispatching. `:tell` is worse still —
/// steering a coordinator after the fact is often simply useless.
///
/// Returns the normalized command word when `line` is one of them.
pub fn runs_immediately(line: &str) -> Option<&'static str> {
    let rest = line.trim().strip_prefix(':')?;
    // The command word is everything up to the first whitespace; arguments are
    // irrelevant to the classification.
    let word = rest.split_whitespace().next().unwrap_or("");
    // Keep this list NARROW and explicit — an accidental entry that needs
    // `&mut Session` would deadlock or (worse) race the live turn.
    //
    // Deliberately NOT here, despite qualifying on property (1):
    //   · `:jobs` / `:status` — these PRINT, and racing the live turn's output
    //     and footer for a listing nobody is blocked on isn't worth it.
    const IMMEDIATE: &[&str] = &[
        "dispatch",
        // `:tell` + aliases. Steering is the case that needs immediacy MOST: a
        // correction folded in after the turn lands is one the coordinator has
        // already blown past, which is exactly the bug this module exists for.
        "tell",
        "msg",
        "send",
        // `:stop` + aliases. Standing down a runaway coordinator you wait for is
        // a coordinator you keep paying for — the opposite of standing it down.
        "stop",
        "standdown",
        "stand-down",
        // `:workers`. The listing of what's running is the ONE print worth the
        // output-area race: you type it mid-turn precisely BECAUSE work is in
        // flight, and queueing it means the table you asked for lands after the
        // turn — showing a different, already-moved-on world. It renders off
        // `OpsCtx` via `repl::workers_listing_ctx`, so no `&mut Session`. The
        // interactive keyboard modal is NOT a hazard here: `handle_colon` gates
        // it behind `!keywatch::installed()`, and keywatch owns stdin for the
        // whole turn, so the mid-turn path is always the static table.
        "workers",
    ];
    let word = IMMEDIATE.iter().copied().find(|c| *c == word)?;
    // One carve-out: `:tell goal <msg>` is not a coordinator message at all — it
    // routes into `steer_active_goal`, which DOES need `&mut Session` (goal state
    // is not in `OpsCtx`). That single form stays queued; the live turn owns the
    // session. Exact-match the target so `:tell goalkeeper …` still runs now.
    if matches!(word, "tell" | "msg" | "send") {
        let mut args = rest.split_whitespace().skip(1);
        let mut target = args.next();
        // A leading `--any`/`-a` is the cross-session override, not the target.
        if matches!(target, Some("--any") | Some("-a")) {
            target = args.next();
        }
        if target == Some(crate::worker::GOAL_STREAM_LABEL) {
            return None;
        }
    }
    Some(word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immediate_classifies_local_only_colon_commands() {
        assert_eq!(runs_immediately(":dispatch do a thing"), Some("dispatch"));
        // Surrounding whitespace is incidental, not meaningful.
        assert_eq!(runs_immediately("  :dispatch go  "), Some("dispatch"));
        // Bare `:dispatch` still classifies — the usage/goal-suggestion message
        // is the command's own business, not the classifier's.
        assert_eq!(runs_immediately(":dispatch"), Some("dispatch"));
        // `:workers` — asked for mid-turn precisely BECAUSE work is in flight,
        // so it must run NOW, in every argument form, rather than render a
        // post-turn world.
        assert_eq!(runs_immediately(":workers"), Some("workers"));
        assert_eq!(runs_immediately(":workers all"), Some("workers"));
        assert_eq!(runs_immediately(":workers diagram"), Some("workers"));
        assert_eq!(runs_immediately("  :workers  "), Some("workers"));
        // ...but a prefix collision is a different command entirely.
        assert_eq!(runs_immediately(":workersfoo"), None);
    }

    #[test]
    fn immediate_rejects_prose_and_session_mutating_commands() {
        // Prose is model-bound — must stay queued.
        assert_eq!(runs_immediately("what is the sprint status"), None);
        assert_eq!(runs_immediately(""), None);
        assert_eq!(runs_immediately("dispatch without the colon"), None);
        // `&mut Session` commands must stay queued (the live turn holds it).
        assert_eq!(runs_immediately(":model opus"), None);
        assert_eq!(runs_immediately(":clear"), None);
        assert_eq!(runs_immediately(":restart"), None);
        assert_eq!(runs_immediately(":quit"), None);
        // Still-excluded prints — see the exclusion note in `runs_immediately`.
        assert_eq!(runs_immediately(":jobs"), None);
        assert_eq!(runs_immediately(":status"), None);
        // `:tell goal …` routes into `steer_active_goal`, which needs the
        // session — the one steering form that must still queue.
        assert_eq!(runs_immediately(":tell goal narrow the scope"), None);
        assert_eq!(runs_immediately(":tell --any goal narrow it"), None);
        assert_eq!(runs_immediately(":msg -a goal narrow it"), None);
        // Prefix collisions must not match.
        assert_eq!(runs_immediately(":dispatchfoo"), None);
        assert_eq!(runs_immediately(":dispatch-stats"), None);
    }

    fn keys(bytes: &[u8]) -> Vec<Key> {
        KeyParser::new().decode(bytes)
    }

    #[test]
    fn plain_ascii_text() {
        assert_eq!(keys(b"hi"), vec![Key::Char('h'), Key::Char('i')]);
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

    // ---- bracketed paste (GH: pasting while busy queued EACH line) ----

    /// Drive a byte stream through one parser + one buffer, returning the
    /// pending line and every action the keys produced — the exact pipeline the
    /// mid-turn reader runs.
    fn drive(chunks: &[&[u8]]) -> (String, Vec<Action>) {
        let mut p = KeyParser::new();
        let mut b = LineBuf::new();
        let mut acts = Vec::new();
        for chunk in chunks {
            for k in p.decode(chunk) {
                acts.push(b.apply(k));
            }
        }
        (b.as_string(), acts)
    }

    #[test]
    fn bracketed_paste_is_one_edit_not_one_submit_per_line() {
        let (line, acts) = drive(&[b"\x1b[200~one\ntwo\nthree\x1b[201~".as_slice()]);
        // THE bug: three pasted lines must not become three queued commands.
        assert!(!acts.iter().any(|a| matches!(a, Action::Submit(_))));
        assert_eq!(acts.len(), 1, "a paste is exactly one key");
        assert_eq!(line, "one\ntwo\nthree");
    }

    #[test]
    fn paste_then_enter_submits_the_whole_block_once() {
        let mut p = KeyParser::new();
        let mut b = LineBuf::new();
        for k in p.decode(b"\x1b[200~a\nb\x1b[201~") {
            assert_eq!(b.apply(k), Action::None);
        }
        // The operator still owns submission: paste → Enter.
        assert_eq!(b.apply(Key::Enter), Action::Submit("a\nb".to_string()));
        assert!(b.is_empty());
    }

    #[test]
    fn typing_around_a_paste_lands_at_the_cursor() {
        // input → paste → input → Enter is a valid operator sequence.
        let (line, acts) = drive(&[
            b"go ".as_slice(),
            b"\x1b[200~x\ny\x1b[201~".as_slice(),
            b"!".as_slice(),
        ]);
        assert!(!acts.iter().any(|a| matches!(a, Action::Submit(_))));
        assert_eq!(line, "go x\ny!");
    }

    #[test]
    fn paste_split_across_reads_still_lands_as_one_key() {
        // Payload AND end marker can straddle `read()` chunks.
        let (line, acts) = drive(&[
            b"\x1b[20".as_slice(),
            b"0~first\nsec".as_slice(),
            b"ond\x1b[20".as_slice(),
            b"1~".as_slice(),
        ]);
        assert_eq!(line, "first\nsecond");
        assert_eq!(acts.len(), 1);
    }

    #[test]
    fn paste_normalises_crlf_and_drops_the_trailing_newline() {
        // Copying whole lines almost always grabs the final newline — it must
        // read as end-of-text, never as "submit".
        let (line, acts) = drive(&[b"\x1b[200~win\r\nlines\r\n\x1b[201~".as_slice()]);
        assert_eq!(line, "win\nlines");
        assert!(!acts.iter().any(|a| matches!(a, Action::Submit(_))));
    }

    #[test]
    fn newline_only_paste_is_dropped() {
        let (line, acts) = drive(&[b"\x1b[200~\n\n\x1b[201~".as_slice()]);
        assert!(line.is_empty());
        assert!(acts.is_empty());
    }

    #[test]
    fn unbracketed_multiline_chunk_is_treated_as_a_paste() {
        // Terminals that don't speak DECSET 2004 send no markers; an interior
        // newline cannot have been typed, so it is still ONE edit.
        let (line, acts) = drive(&[b"alpha\nbeta".as_slice()]);
        assert_eq!(line, "alpha\nbeta");
        assert!(!acts.iter().any(|a| matches!(a, Action::Submit(_))));
    }

    #[test]
    fn typed_enter_still_submits_and_stray_end_marker_is_noise() {
        // A chunk whose only newline is terminal is a human pressing Enter.
        let mut p = KeyParser::new();
        let mut b = LineBuf::new();
        let mut acts = Vec::new();
        for k in p.decode(b"ls -la\r") {
            acts.push(b.apply(k));
        }
        assert_eq!(acts.last(), Some(&Action::Submit("ls -la".to_string())));
        // An orphan `ESC [201~` in ground state decodes to nothing at all.
        assert!(p.decode(b"\x1b[201~").is_empty());
    }

    #[test]
    fn paste_display_keeps_the_block_on_one_row() {
        let mut p = KeyParser::new();
        let mut b = LineBuf::new();
        for k in p.decode(b"\x1b[200~a\nb\x1b[201~") {
            b.apply(k);
        }
        // Painted single-row — the painters flatten on the way out…
        assert_eq!(crate::terminal::flatten_rows(&b.as_string(), true), "a⏎b");
        // …but submitted with the real newlines intact.
        assert_eq!(b.as_string(), "a\nb");
    }

    // ---- history recall (GH: Up was inert while aish was busy) ----

    #[test]
    fn up_recalls_newest_then_older() {
        let mut b = LineBuf::with_history(vec!["first".into(), "second".into()]);
        b.apply(Key::Up);
        assert_eq!(b.as_string(), "second");
        assert_eq!(b.cursor(), "second".chars().count());
        b.apply(Key::Up);
        assert_eq!(b.as_string(), "first");
        // Walking past the oldest entry parks there instead of clearing.
        b.apply(Key::Up);
        assert_eq!(b.as_string(), "first");
    }

    #[test]
    fn down_walks_back_and_restores_draft() {
        let mut b = LineBuf::with_history(vec!["older".into(), "newer".into()]);
        for c in "draft".chars() {
            b.apply(Key::Char(c));
        }
        b.apply(Key::Up);
        assert_eq!(b.as_string(), "newer");
        b.apply(Key::Up);
        assert_eq!(b.as_string(), "older");
        b.apply(Key::Down);
        assert_eq!(b.as_string(), "newer");
        // Past the newest entry the operator's own in-progress line comes back.
        b.apply(Key::Down);
        assert_eq!(b.as_string(), "draft");
        // Down at the live line is a no-op, not a wipe.
        b.apply(Key::Down);
        assert_eq!(b.as_string(), "draft");
    }

    #[test]
    fn history_is_deduped_and_blank_filtered() {
        let b = LineBuf::with_history(vec![
            "ls".into(),
            "ls".into(),
            "   ".into(),
            "".into(),
            "git status".into(),
        ]);
        let mut b = b;
        b.apply(Key::Up);
        assert_eq!(b.as_string(), "git status");
        b.apply(Key::Up);
        assert_eq!(b.as_string(), "ls");
        b.apply(Key::Up);
        assert_eq!(b.as_string(), "ls");
    }

    #[test]
    fn up_without_history_is_inert() {
        let mut b = typed("abc");
        assert_eq!(b.apply(Key::Up), Action::None);
        assert_eq!(b.as_string(), "abc");
        assert_eq!(b.apply(Key::Down), Action::None);
        assert_eq!(b.as_string(), "abc");
    }

    #[test]
    fn submitted_line_becomes_recallable() {
        let mut b = LineBuf::with_history(vec!["old".into()]);
        for c in "fresh".chars() {
            b.apply(Key::Char(c));
        }
        assert_eq!(b.apply(Key::Enter), Action::Submit("fresh".to_string()));
        b.apply(Key::Up);
        assert_eq!(b.as_string(), "fresh");
        b.apply(Key::Up);
        assert_eq!(b.as_string(), "old");
    }

    #[test]
    fn editing_a_recalled_line_detaches_from_history() {
        let mut b = LineBuf::with_history(vec!["cargo test".into()]);
        b.apply(Key::Up);
        assert_eq!(b.as_string(), "cargo test");
        b.apply(Key::Char('!'));
        assert_eq!(b.as_string(), "cargo test!");
        // Editing re-owns the line: Down must not clobber the edit with the
        // next history entry (there is none) — it restores the empty draft only
        // after the edit is submitted, so here Down is a no-op on the edit.
        b.apply(Key::Down);
        assert_eq!(b.as_string(), "cargo test!");
        // Submitting the edited line records the edit, not the original.
        assert_eq!(
            b.apply(Key::Enter),
            Action::Submit("cargo test!".to_string())
        );
    }

    // ---- FooterRender ----

    #[test]
    fn footer_draw_is_blank_gap_then_prompt() {
        let f = FooterRender {
            prompt: "» ",
            text: "hi",
            cols: 80,
        };
        assert_eq!(f.draw(), "\n» hi");
    }

    #[test]
    fn footer_single_row_erase_is_inverse() {
        let f = FooterRender {
            prompt: "» ",
            text: "hi",
            cols: 80,
        };
        assert_eq!(f.prompt_rows(), 1);
        // clear prompt row, then up-and-clear the blank gap line.
        assert_eq!(f.erase(), "\r\x1b[2K\x1b[1A\x1b[2K");
    }

    #[test]
    fn footer_empty_text_still_one_row() {
        let f = FooterRender {
            prompt: "» ",
            text: "",
            cols: 80,
        };
        assert_eq!(f.prompt_rows(), 1);
        assert_eq!(f.draw(), "\n» ");
    }

    #[test]
    fn footer_unknown_width_assumes_single_row() {
        let f = FooterRender {
            prompt: "» ",
            text: "a very long line that would wrap",
            cols: 0,
        };
        assert_eq!(f.prompt_rows(), 1);
    }

    #[test]
    fn footer_wraps_to_multiple_rows() {
        // prompt width 2 + text width 10 = 12 cols; at width 6 → 12/6 + 1 = 3 rows.
        let f = FooterRender {
            prompt: "» ",
            text: "0123456789",
            cols: 6,
        };
        assert_eq!(f.prompt_rows(), 3);
        // erase clears the last prompt row then walks up over 3 more lines
        // (2 remaining prompt rows + the blank gap).
        assert_eq!(
            f.erase(),
            "\r\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2K"
        );
    }

    #[test]
    fn footer_wide_chars_count_as_two_columns() {
        // Two double-width chars (width 4) + prompt width 2 = 6 at cols 6 → 2 rows.
        let f = FooterRender {
            prompt: "» ",
            text: "中文",
            cols: 6,
        };
        assert_eq!(f.prompt_rows(), 2);
    }
}
