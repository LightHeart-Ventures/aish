//! Host-side subprocess background workers — full-tool deferrable jobs.
//!
//! Where `batch.rs` offloads a tool-LESS task to the Anthropic Batches API, this
//! re-execs aish itself as a child process in `--coordinator` mode. The child
//! runs the full agentic tool loop (filesystem, run_program, MCP) in the SAME
//! cwd and inherits the parent's environment, so it has exactly the tools and
//! MCP servers the interactive session has. It prints its final answer to
//! stdout; we capture that as the result and surface it the same way batch
//! results land (`on_complete` → `flush_results`).
//!
//! No Docker: the child is a plain host subprocess. Isolation is the same trust
//! model as interactive aish (which already runs arbitrary commands on the
//! host). What the subprocess buys over an in-process task is an INDEPENDENT
//! `Session` — its own history, cwd, and MCP connections — so a background job
//! can't corrupt the live session's state.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use uuid::Uuid;

/// How long a worker may run before it's killed. Generous — these are deferred,
/// possibly-long jobs — but bounded so a wedged child can't live forever.
const WORKER_TIMEOUT: Duration = Duration::from_secs(60 * 60); // 1h

/// Max bytes of a child's stdout/stderr we keep. A runaway coordinator that
/// dumps gigabytes must never OOM the PARENT (the interactive aish / goal loop)
/// via an unbounded read — so we cap the capture and drain the rest.
const CAPTURE_CAP: usize = 1024 * 1024; // 1 MB

/// Read up to `cap` bytes of `r` into a String, then keep draining (so the
/// child never blocks on a full pipe) but discard the overflow. A truncation
/// marker is appended if it overflowed.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut r: R, cap: usize) -> String {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    let mut overflowed = false;
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = n.min(cap - buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                    overflowed |= take < n;
                } else {
                    overflowed = true; // past the cap — keep draining, drop the bytes
                }
            }
        }
    }
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if overflowed {
        s.push_str("\n…[output truncated — exceeded the capture cap]");
    }
    s
}

// ---------------------------------------------------------------------------
// Prompt-badge pulse — colour the ⟳N indicator by recent background activity
// ---------------------------------------------------------------------------

/// How long a background-worker event keeps the prompt's `⟳N` badge pulsing
/// (coloured glyph) before it fades back to the idle dim `⟳N`. Short enough to
/// read as a transient "pulse", long enough to be seen at the next prompt draw.
pub const PULSE_FADE: Duration = Duration::from_millis(900);

/// A single prompt-badge pulse event, derived from a coordinator's stderr.
/// Most-recent-wins across all live workers (see [`fresh_pulse`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pulse {
    /// A tool call finished successfully — pulse green ✓.
    ToolOk,
    /// A tool call failed — pulse red ✗.
    ToolErr,
    /// The model emitted a turn/narration line — pulse magenta ⟳.
    Turn,
}

/// True when a coordinator stderr line carries a tool-activity glyph: the 🔧
/// wrench (MCP tool call), the 🛠️ hammer-and-wrench (local tool/exe/script), or
/// the 🤝 handshake (an `escalate` model hand-off). These are the glyphs
/// `engine::tool_glyph` stamps between the status icon and the desc, so a line
/// carrying any of them is a tool line (start or result).
fn has_tool_glyph(line: &str) -> bool {
    line.contains('🔧') || line.contains('🛠') || line.contains('🤝')
}

/// Classify ONE raw coordinator-stderr line into a prompt-badge pulse event, or
/// `None` when it carries no event. Pure, so it's unit-testable without a pipe.
///
/// The coordinator runs non-TTY, so its post-execution tool line is the static
/// `✓/✗ 🔧 <desc>` shape (see `engine::tool_result_line`): a `✓` glyph alongside
/// the wrench is a success, a `✗` a failure. A bare `🔧 <desc>` START line (no
/// status glyph) is the tool *beginning*, not an outcome → `None`. A `🗨` line is
/// turn narration (`engine::emit_narration`) → a turn-completion pulse.
fn classify_event(line: &str) -> Option<Pulse> {
    if has_tool_glyph(line) {
        if line.contains('✓') {
            return Some(Pulse::ToolOk);
        }
        if line.contains('✗') {
            return Some(Pulse::ToolErr);
        }
        return None; // a bare start line — no outcome yet
    }
    if line.trim_start().starts_with('🗨') {
        return Some(Pulse::Turn);
    }
    None
}

/// How many of the child's most-recent stderr lines we retain for the failure
/// message. We stream stderr live rather than accumulating it (an unbounded
/// accumulation was the OOM risk), so the failure path can only quote a bounded
/// tail rather than the whole thing.
const STDERR_TAIL_LINES: usize = 20;

/// Max number of forwardable activity lines retained per worker for an
/// `:attach` backfill replay. Bounds the per-worker transcript so a chatty
/// coordinator can't grow the PARENT's memory without limit (the same OOM
/// discipline as `read_capped`). Paired with [`TRANSCRIPT_MAX_BYTES`];
/// whichever cap trips first evicts the oldest rows.
const TRANSCRIPT_MAX_LINES: usize = 1000;

/// Byte budget for the retained per-worker transcript (see
/// [`TRANSCRIPT_MAX_LINES`]). Oldest rows are evicted once the running total
/// of `(suffix + text)` bytes exceeds this.
const TRANSCRIPT_MAX_BYTES: usize = 256 * 1024;

/// Decide whether a single raw coordinator-stderr line is worth forwarding to
/// the user's terminal, and if so return the cleaned text to forward.
///
/// The coordinator runs non-TTY, so a tool emits TWO static lines per call (see
/// `engine::ToolSpinner`): a bare `🔧 <desc>` START line at `start`, then a
/// `✓/✗ 🔧 <desc>` RESULT line at `finish` (the latter is what `classify_event`
/// reads to drive the prompt-badge pulse). Forwarding BOTH made every tool call
/// appear TWICE in the `:worker-output` stream — the duplicate tool-call logging
/// bug. We now forward ONLY the RESULT line (the one carrying the `✓/✗` outcome)
/// so each tool call is logged exactly once; the bare START line is dropped here
/// (the badge pulse still fires from the RESULT line in `stream_stderr`,
/// independent of this gate). The "coordinator run … starting" banner and blank
/// lines carry no wrench and are dropped too. Cleaning strips leading whitespace
/// and the outer dim `\x1b[2m…\x1b[0m` wrapper, so `announce` (which re-wraps in
/// dim) doesn't double-wrap.
fn clean_activity_line(raw: &str) -> Option<String> {
    if !has_tool_glyph(raw) {
        return None;
    }
    // Forward only the RESULT line (it carries the ✓/✗ outcome). The bare START
    // line — a wrench with no status glyph — is the tool *beginning*; forwarding
    // it as well is exactly the duplicate. Drop it.
    if !raw.contains('✓') && !raw.contains('✗') {
        return None;
    }
    let mut s = raw.trim();
    // Strip the dim wrapper the coordinator emits around static tool lines.
    s = s.strip_prefix("\x1b[2m").unwrap_or(s);
    s = s.strip_suffix("\x1b[0m").unwrap_or(s);
    let cleaned = s.trim();
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned.to_string())
}

/// A coordinator stderr line carrying turn text (the `🗨` sentinel emitted by
/// `engine::emit_narration`) or a batch-phase notice (`📦` from the coordinator
/// loop). Returns the cleaned text after the sentinel, or `None`.
fn strip_sentinel(raw: &str, mark: &str) -> Option<String> {
    let rest = raw.trim_start().strip_prefix(mark)?.trim_start();
    (!rest.is_empty()).then(|| rest.to_string())
}

/// A coordinator's `message_console` note: the `📣` sentinel line emitted by the
/// `message_console` tool (see `tools::message_console`). Unlike every other
/// forwarded line, a console message ALWAYS reaches the operator's terminal — it
/// bypasses the `:worker-output` suppression gate — so the stream loop matches it
/// FIRST, before the normal forward/suppress logic. Returns the cleaned note text
/// after the sentinel, or `None`. Pure → unit-testable without a pipe.
fn console_message(raw: &str) -> Option<String> {
    strip_sentinel(raw, "📣")
}

/// Decide what (if anything) to forward to the user's terminal for ONE raw
/// coordinator-stderr line, given whether `:worker-output` is on. Returns the
/// cleaned text to announce as `[label] text`, or `None` to drop the line. Pure
/// — the single source of truth for the suppression gate, so it's unit-testable
/// without a live pipe.
///
/// Suppression policy (the default): a background coordinator is QUIET. With
/// `show_output` off NOTHING from its stderr is forwarded — not its tool
/// activity, not its turn narration. The user still sees the job is alive via the
/// prompt's `⟳N` pulse and its completion notice (both independent of this
/// stream); they just don't get the firehose of every tool call. Flipping
/// `:worker-output on` opens the full live stream:
/// - `💭` thinking notice (entered the model-reasoning phase) → `[label] thinking…`
/// - tool activity (the `✓/✗ <glyph>` RESULT line, once per call) → `[label] ✓ <glyph> …`
/// - `🗨` turn text (a standard model call) → `[label]   🚀 …` (glyph aligned under the tool glyph)
/// - `📦` batch fan-out notice → `[label]   🐌 …` (glyph aligned under the tool glyph)
fn forward_decision(line: &str, show_output: bool) -> Option<String> {
    if !show_output {
        // Default: keep background coordinators quiet. The job's liveness is
        // shown by the ⟳N prompt pulse + completion notice, not this stream.
        return None;
    }
    if let Some(activity) = clean_activity_line(line) {
        // The coordinator already stamped the single source glyph between the
        // status icon and the desc (🔧 MCP · 🛠️ local · 🤝 escalate), so the
        // RESULT line is forwarded VERBATIM — no extra decoration, so a line
        // never carries two source glyphs.
        return Some(activity);
    }
    if let Some(text) = strip_sentinel(line, "💭") {
        // Thinking notice: the 💭 glyph rides AFTER the worker-id gutter, padded
        // by NARRATION_ALIGN_PAD so it lines up under the tool glyph / 🚀 rocket
        // (the shared "rocket alignment" column) → `[label]   💭 …`.
        return Some(format!("{NARRATION_ALIGN_PAD}💭 {text}"));
    }
    if let Some(text) = strip_sentinel(line, "🗨") {
        // Turn narration: the 🚀 rocket rides AFTER the worker-id gutter, as a
        // prefix to the text. The 2-col NARRATION_ALIGN_PAD stands in for the
        // `✓ ` status mark on a tool RESULT line so the rocket lines up under
        // the tool glyph → `[label]   🚀 …`.
        return Some(format!("{NARRATION_ALIGN_PAD}🚀 {text}"));
    }
    if let Some(text) = strip_sentinel(line, "📦") {
        // Batch fan-out: the 🐌 marker prefixes the text, padded by
        // NARRATION_ALIGN_PAD so it aligns under the tool glyph → `[label]   🐌 …`.
        return Some(format!("{NARRATION_ALIGN_PAD}🐌 {text}"));
    }
    None
}

// ---------------------------------------------------------------------------
// Contained `:output` pane — frame streamed coordinator activity (w_sn1fHhd5)
// ---------------------------------------------------------------------------
//
// `:output on` streams a background coordinator's live activity to the user's
// terminal. Without containment those lines blend into the user's own shell
// scroll, indistinguishable from interactive command output. A line-streaming
// REPL can't carve a fixed split-screen region for them without a full TUI
// takeover, and several coordinators interleave their lines concurrently — so
// the coherent "pane" is a box-drawing LEFT BORDER carried by every forwarded
// row: a bordered side-column that visually groups the coordinator stream and
// sets it apart from interactive output, with a top/bottom frame bracketing the
// region when the pane is opened (`:output on`) and closed (`:output off`).
// Every row self-identifies with its `[label]` gutter and lines up under
// the one shared border, so interleaving stays readable.

/// The cyan box-drawing left border every pane row carries — the pane's "wall".
/// Used for the global `:worker-output` stream and for an `:attach` backfill
/// (the cyan "output to date" replay).
const PANE_BORDER: &str = "\x1b[36m┃\x1b[0m";

/// The GREEN left border for a worker's LIVE stream once you've `:attach`ed to
/// it. It marks NEW output produced AFTER the attach point, distinguishing fresh
/// activity from the cyan "output to date" replay (`pane_replay_header` + the
/// backfilled tail) that precedes it. Same glyph, different colour — so the
/// stream reads as one continuous column that flips cyan → green at the live edge.
const PANE_BORDER_LIVE: &str = "\x1b[32m┃\x1b[0m";

/// Two-column pad that aligns a turn/batch narration glyph (🚀/🐌)
/// under the tool glyph (🛠️/🔧) on the preceding RESULT lines. A tool
/// RESULT line renders as `✓ <glyph> …` — a 1-column status mark plus a space (2
/// columns) precede its glyph — whereas a narration line puts its glyph right
/// after the `[label]` gutter. Prefixing narration with these two spaces lines
/// the rocket/snail up in the same column as the tool glyph above it.
const NARRATION_ALIGN_PAD: &str = "  ";

/// Render one forwarded coordinator line as a row of the contained `:output`
/// pane: `┃ [label] text`. The border + `[label]` gutter are chrome (cyan
/// border, dim label); `text` is emitted verbatim so it keeps whatever inline
/// colour the coordinator produced — the green `✓`/red `✗` on a tool RESULT
/// line, the `🛠️`/`🔧` source glyph, the turn/batch narration. The shared left
/// border on every row is what CONTAINS the stream as a bordered column distinct
/// from the user's interleaved shell output. Pure — unit-tested.
/// Max leading visible columns of a body treated as the glyph/status "prefix"
/// (e.g. `✓ 🛠️ ` or the 2-col NARRATION_ALIGN_PAD + `🚀 `). Continuation lines
/// hang-indent under the column right after this prefix — i.e. under the first
/// letter of the message. Capped so a real message that merely opens with a
/// glyph can never be swallowed wholesale.
const MSG_INDENT_CAP: usize = 6;

/// Below this many columns available for the message, don't hang-indent — the
/// sliver would look worse than letting the terminal soft-wrap the whole row.
const MIN_WRAP_COLS: usize = 24;

/// Terminal width for pane wrapping. A tty (stderr preferred — that's where pane
/// rows print — then stdout) is queried via TIOCGWINSZ. Off a tty we honor an
/// explicit `$COLUMNS` and otherwise return `usize::MAX` so captured/piped output
/// (and unit tests) is emitted as a single line, byte-identical to before — the
/// wrapping only kicks in when a real terminal renders it.
fn pane_cols() -> usize {
    // SAFETY: isatty + a read-only TIOCGWINSZ ioctl on fd 2 / fd 1.
    unsafe {
        for fd in [2, 1] {
            if libc::isatty(fd) == 1 {
                let mut ws: libc::winsize = std::mem::zeroed();
                if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
                    return ws.ws_col as usize;
                }
            }
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(usize::MAX)
}

/// Visible display width of `s` with ANSI SGR sequences discounted (zero-width).
///
/// The visible remainder is measured with `UnicodeWidthStr::width` (string-level,
/// not a per-char sum) so an emoji-presentation sequence — a base codepoint
/// followed by U+FE0F, e.g. `🛠️` / `⚙️` / `✏️` — is counted at its true 2-column
/// terminal width. Summing `UnicodeWidthChar::width` per char undercounts those
/// by one (the base is width-1, the VS16 selector width-0), and that off-by-one
/// is exactly what pushed a wrapped row one column past the margin and made the
/// terminal soft-wrap its last character onto a stray line.
fn vis_cols(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    let mut clean = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip a CSI escape: ESC '[' … final alpha byte.
            if chars.next() == Some('[') {
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        clean.push(c);
    }
    UnicodeWidthStr::width(clean.as_str())
}

/// Terminal width of a single `char` while stepping a string, promoted to 2 when
/// it is an emoji base immediately followed by U+FE0F (the emoji-presentation
/// selector, passed as `next`). `unicode-width`'s per-char width can't see the
/// trailing selector and undercounts `🛠️`/`⚙️`/… by one column; this restores the
/// true width for the char-by-char hard-break loops. The VS16 selector itself is
/// width-0, so the pair still totals 2. Pure → unit-tested.
fn step_width(c: char, next: Option<char>) -> usize {
    use unicode_width::UnicodeWidthChar;
    let base = UnicodeWidthChar::width(c).unwrap_or(0);
    if base == 1 && next == Some('\u{FE0F}') {
        2
    } else {
        base
    }
}

/// Is `c` part of a leading glyph/status prefix (not message text)? Covers the
/// status marks (✓/✗), the source & narration glyphs (🛠 🔧 🤝 🚀 🐌 💬 💭 …),
/// their VS16/ZWJ joiners, the braille spinner frames, and spaces/pad.
fn is_glyph_skip(c: char) -> bool {
    matches!(c,
        ' '
        | '\u{2713}' | '\u{2717}'            // ✓ ✗
        | '\u{FE0F}' | '\u{200D}'            // VS16, ZWJ
    )
        || ('\u{2800}'..='\u{28FF}').contains(&c)   // braille spinner frames
        || ('\u{2600}'..='\u{27BF}').contains(&c)   // misc symbols & dingbats
        || ('\u{1F300}'..='\u{1FAFF}').contains(&c) // emoji (🚀 🐌 💬 💭 🔧 🛠 🤝 …)
}

/// Split a pane body into its leading glyph/status PREFIX and the MESSAGE that
/// follows. ANSI SGR runs are always folded into the prefix (zero visible
/// width); glyph/space codepoints are folded up to `MSG_INDENT_CAP` visible
/// columns. The message is what a wrapped continuation line hang-indents under.
fn split_body_glyph(body: &str) -> (&str, &str) {
    use unicode_width::UnicodeWidthChar;
    let mut vis = 0usize;
    let mut cut = 0usize;
    let mut it = body.char_indices().peekable();
    while let Some(&(_, c)) = it.peek() {
        if c == '\x1b' {
            it.next();
            while let Some(&(_, cc)) = it.peek() {
                it.next();
                if cc.is_ascii_alphabetic() {
                    break;
                }
            }
            cut = it.peek().map(|&(k, _)| k).unwrap_or(body.len());
            continue;
        }
        if is_glyph_skip(c) {
            let w = UnicodeWidthChar::width(c).unwrap_or(0);
            if vis + w > MSG_INDENT_CAP {
                break;
            }
            vis += w;
            it.next();
            cut = it.peek().map(|&(k, _)| k).unwrap_or(body.len());
            continue;
        }
        break;
    }
    (&body[..cut], &body[cut..])
}

/// Word-wrap plain `msg` into chunks each ≤ `width` visible columns. Prefers
/// breaking at spaces; hard-breaks a single word longer than `width` by display
/// column. Callers guard against ANSI-bearing messages (which must not wrap).
fn wrap_visible(msg: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;
    let width = width.max(1);
    // String-level width so emoji-presentation sequences (base + U+FE0F) count
    // as 2 columns, matching the terminal — a per-char sum undercounts them.
    let cw = |s: &str| UnicodeWidthStr::width(s);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in msg.split(' ') {
        let ww = cw(word);
        if !cur.is_empty() && cur_w + 1 + ww > width {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        if ww > width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let mut chunk = String::new();
            let mut chw = 0usize;
            let mut it = word.chars().peekable();
            while let Some(ch) = it.next() {
                let w = step_width(ch, it.peek().copied());
                if chw + w > width && !chunk.is_empty() {
                    lines.push(std::mem::take(&mut chunk));
                    chw = 0;
                }
                chunk.push(ch);
                chw += w;
            }
            cur = chunk;
            cur_w = chw;
        } else {
            if !cur.is_empty() {
                cur.push(' ');
                cur_w += 1;
            }
            cur.push_str(word);
            cur_w += ww;
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Fold the SGR escape sequences contained in `seg` into `state` — the running
/// set of styles "active" at the cursor, which a wrapped continuation line must
/// re-open so a colour span split across the wrap keeps painting. A full reset
/// (`\x1b[0m` / `\x1b[m`) clears the set; every other SGR sequence is appended
/// (later codes override earlier ones for the same attribute, so re-emitting the
/// whole chain reproduces the exact terminal state). Non-SGR escapes and plain
/// text are ignored. Pure → unit-tested.
fn absorb_sgr(state: &mut String, seg: &str) {
    let mut it = seg.char_indices().peekable();
    while let Some(&(start, c)) = it.peek() {
        if c != '\x1b' {
            it.next();
            continue;
        }
        it.next(); // ESC
        if !matches!(it.peek(), Some(&(_, '['))) {
            continue; // not a CSI — ignore
        }
        it.next(); // '['
        let mut end = start + 1;
        let mut final_byte = '\0';
        while let Some(&(k, cc)) = it.peek() {
            it.next();
            end = k + cc.len_utf8();
            if cc.is_ascii_alphabetic() {
                final_byte = cc;
                break;
            }
        }
        if final_byte == 'm' {
            let seq = &seg[start..end];
            if seq == "\x1b[0m" || seq == "\x1b[m" {
                state.clear();
            } else {
                state.push_str(seq);
            }
        }
    }
}

/// Like [`wrap_visible`], but for a message that CARRIES inline ANSI SGR colour
/// (the markdown-rendered narration the coordinator emits — `code`/**bold**
/// spans arrive pre-coloured). Wraps on VISIBLE columns (escapes are zero-width)
/// and returns self-contained chunks: each re-opens the SGR state active at its
/// start (seeded from `seed`, whatever the glyph prefix left open) and appends a
/// reset, so a colour span split across a wrap never bleeds into the pane border,
/// the hang-indent pad, or the rest of the terminal. Pure → unit-tested.
fn wrap_visible_ansi(msg: &str, width: usize, seed: &str) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut open = seed.to_string(); // SGR active at the START of the current line
    let mut state = seed.to_string(); // SGR active at the cursor (end of `cur`)
    let mut cur = String::new();
    let mut cur_w = 0usize;

    // Emit `open + cur (+ reset)` as a finished line, then start a fresh line
    // whose opening SGR is whatever is active NOW (`state`).
    fn finish(
        lines: &mut Vec<String>,
        open: &mut String,
        state: &str,
        cur: &mut String,
        cur_w: &mut usize,
    ) {
        let mut s = String::with_capacity(open.len() + cur.len() + 4);
        s.push_str(open);
        s.push_str(cur);
        if !open.is_empty() || !state.is_empty() {
            s.push_str("\x1b[0m");
        }
        lines.push(s);
        *open = state.to_string();
        cur.clear();
        *cur_w = 0;
    }

    for word in msg.split(' ') {
        let ww = vis_cols(word);
        if cur_w > 0 && cur_w + 1 + ww > width {
            finish(&mut lines, &mut open, &state, &mut cur, &mut cur_w);
        }
        if ww > width {
            // A single word wider than the line: hard-break it by display column,
            // stepping whole ANSI escapes (zero width) so none is ever split.
            if cur_w > 0 {
                finish(&mut lines, &mut open, &state, &mut cur, &mut cur_w);
            }
            let mut it = word.char_indices().peekable();
            while let Some(&(start, c)) = it.peek() {
                if c == '\x1b' {
                    it.next(); // ESC
                    let mut end = start + 1;
                    if matches!(it.peek(), Some(&(_, '['))) {
                        it.next(); // '['
                        while let Some(&(k, cc)) = it.peek() {
                            it.next();
                            end = k + cc.len_utf8();
                            if cc.is_ascii_alphabetic() {
                                break;
                            }
                        }
                    }
                    let seq = &word[start..end];
                    cur.push_str(seq);
                    absorb_sgr(&mut state, seq);
                } else {
                    it.next(); // consume c
                    // Peek the next codepoint so a base + U+FE0F emoji-presentation
                    // pair (e.g. 🛠️) is measured at its true 2-column width.
                    let next = it.peek().map(|&(_, cc)| cc);
                    let w = step_width(c, next);
                    if cur_w + w > width && cur_w > 0 {
                        finish(&mut lines, &mut open, &state, &mut cur, &mut cur_w);
                    }
                    cur.push(c);
                    cur_w += w;
                }
            }
        } else {
            if cur_w > 0 {
                cur.push(' ');
                cur_w += 1;
            }
            cur.push_str(word);
            cur_w += ww;
            absorb_sgr(&mut state, word);
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        finish(&mut lines, &mut open, &state, &mut cur, &mut cur_w);
    }
    lines
}

/// Render one forwarded coordinator line as a row of the contained `:output`
/// pane: `┃ [label] text`. The border + `[label]` gutter are chrome (cyan
/// border, dim label); `text` is emitted verbatim so it keeps whatever inline
/// colour the coordinator produced — the green `✓`/red `✗` on a tool RESULT
/// line, the `🛠️`/`🔧` source glyph, the turn/batch narration. The shared left
/// border on every row is what CONTAINS the stream as a bordered column distinct
/// from the user's interleaved shell output.
///
/// When the row is too wide for the terminal, the message is HANG-INDENTED: it
/// wraps onto continuation rows that carry the same cyan border and are padded so
/// each wrapped line begins under the FIRST LETTER of the message on the opening
/// row (the column right after the glyph prefix). Off a tty (piped/tests) the
/// width is unknown → the row is returned as a single line, unchanged. Pure —
/// unit-tested.
pub fn pane_row(label: &str, text: &str) -> String {
    pane_row_cols(label, text, pane_cols(), PANE_BORDER)
}

/// Like [`pane_row`], but with the GREEN [`PANE_BORDER_LIVE`] wall — used for a
/// worker's LIVE stream after you `:attach` to it, so new activity is set apart
/// from the cyan "output to date" replay. Same layout/wrapping as `pane_row`.
pub fn pane_row_live(label: &str, text: &str) -> String {
    pane_row_cols(label, text, pane_cols(), PANE_BORDER_LIVE)
}

/// Display columns available for CONTENT inside a pane row, after the
/// `┃ [label] ` gutter, at the live terminal width. Feed this to
/// `md::render_stdout_within` so markdown (esp. tables) rendered for a pane fits
/// the remaining width and the terminal doesn't hard-wrap the box. Returns
/// `usize::MAX` when the width is unknown (piped/tests) so rendering stays
/// unbounded — byte-identical to the pre-wrap behaviour there.
pub fn pane_content_cols(label: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    match pane_cols() {
        usize::MAX => usize::MAX,
        cols => cols
            .saturating_sub(5 + UnicodeWidthStr::width(label))
            .max(24),
    }
}

/// Width-parameterized core of [`pane_row`] — pure, so wrapping is unit-tested
/// without touching the terminal or global `$COLUMNS`. `cols == usize::MAX`
/// means "unknown width" → never wrap (single line, byte-identical to the
/// pre-wrap behaviour).
fn pane_row_cols(label: &str, text: &str, cols: usize, border: &str) -> String {
    use unicode_width::UnicodeWidthStr;
    let first = format!("{border} \x1b[2m[{label}]\x1b[0m {text}");
    if cols == usize::MAX {
        return first; // unknown width (piped/tests): never wrap
    }
    // Column where the message begins on the opening row:
    //   "┃ [label] " = ┃(1) + space(1) + '['(1) + label + ']'(1) + space(1)
    let gutter_w = 5 + UnicodeWidthStr::width(label);
    let (prefix, message) = split_body_glyph(text);
    let indent = gutter_w + vis_cols(prefix);
    let avail = cols.saturating_sub(indent);
    // Whole row already fits (ANSI is zero-width, so vis_cols is honest), or it's
    // too narrow to hang-indent → single line.
    if gutter_w + vis_cols(text) <= cols || avail < MIN_WRAP_COLS {
        return first;
    }
    // Wrap the message. If it carries inline SGR colour (markdown-rendered
    // narration — `code`/**bold** spans), wrap ANSI-aware so each continuation
    // line re-opens the style active at the wrap point (seeded from whatever the
    // glyph prefix left open) and resets at its end. Otherwise plain wrap.
    let chunks = if prefix.contains('\x1b') || message.contains('\x1b') {
        let mut seed = String::new();
        absorb_sgr(&mut seed, prefix);
        wrap_visible_ansi(message, avail, &seed)
    } else {
        wrap_visible(message, avail)
    };
    if chunks.len() <= 1 {
        return first;
    }
    // Continuation rows: border + pad so the message column lines up under the
    // opening row's first message letter (┃ is 1 col, then indent-1 spaces).
    let cont = format!("{border}{}", " ".repeat(indent.saturating_sub(1)));
    let mut out = format!(
        "{border} \x1b[2m[{label}]\x1b[0m {prefix}{}",
        chunks[0]
    );
    for chunk in &chunks[1..] {
        out.push('\n');
        out.push_str(&cont);
        out.push_str(chunk);
    }
    out
}

/// The pane's TOP frame, printed once when `:output` is switched on so the rows
/// that follow read as a bracketed region rather than loose lines. Pure.
pub fn pane_open() -> String {
    "\x1b[36m┏━ coordinator output \x1b[0m\x1b[2m(:output on — live activity)\x1b[0m\x1b[36m ━━━━━━\x1b[0m".to_string()
}

/// The pane's BOTTOM frame, printed once when `:output` is switched off. Pure.
pub fn pane_close() -> String {
    "\x1b[36m┗━ coordinator output \x1b[0m\x1b[2m(:output off)\x1b[0m\x1b[36m ━━━━━━━━━━━━━━━\x1b[0m".to_string()
}

/// Header printed once above an `:attach` backfill: it brackets the
/// "output to date" replay block that precedes the now-live coordinator
/// stream. Pure — unit-tested.
pub fn pane_replay_header(short: &str) -> String {
    format!(
        "\x1b[36m\u{250f}\u{2501} {short} \u{2014} output to date \x1b[0m\x1b[2m(replay; live activity follows)\x1b[0m\x1b[36m \u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\x1b[0m"
    )
}

/// Render the coordinator's INPUT (the task it was given) as the opening row of
/// an `:attach` replay. This is the START of the conversation, so — unlike the
/// activity rows that follow — it's set apart to be unmistakable: a 💬 speech
/// glyph announces "this is the prompt" and the whole line is bold (`\x1b[1m…
/// \x1b[0m`). Framed as a normal pane row so it still carries the cyan border +
/// `[label]` gutter. Pure — unit-tested.
pub fn pane_input_row(label: &str, task: &str) -> String {
    pane_row(label, &format!("\x1b[1m💬 task: {task}\x1b[0m"))
}

/// Frame a coordinator's `message_console` note for the operator's terminal.
/// Unlike the dim `:output` pane rows — which are gated behind `:worker-output`
/// and styled as quiet chrome — a console message is ALWAYS shown the instant the
/// coordinator sends it, so it is styled to STAND OUT and read unmistakably as a
/// direct message from that worker: a bright 📣 megaphone, the worker's `[label]`,
/// and the note itself. Carries no pane border (it is deliberately NOT part of
/// the contained activity stream — it's an out-of-band interjection). Pure —
/// unit-tested.
pub fn console_row(label: &str, text: &str) -> String {
    format!("\x1b[1;36m📣 [{label}]\x1b[0m {text}")
}

/// Stream a child's stderr line by line, forwarding the interesting lines to the
/// user's terminal live via `announce`, and retaining only the last
/// `STDERR_TAIL_LINES` raw lines as a bounded ring for the failure message.
/// Returns the retained tail joined with newlines (oldest-first).
///
/// Forwarding is decided per line by [`forward_decision`], which gates ALL
/// coordinator output (tool `🔧` lines included) behind the `:worker-output`
/// toggle (`show_output`). Default (off) → a quiet background job; on → the full
/// live `🔧`/`🗨`/`📦` stream. The toggle is read PER LINE, so flipping it
/// mid-run takes effect on the next line.
///
/// The bounded tail is retained for EVERY line regardless of forwarding, so a
/// failure message can quote recent stderr even when output is suppressed. This
/// keeps the child's stderr pipe drained (so it never blocks) without
/// accumulating all of stderr in memory.
/// Whether a coordinator stderr line should be forwarded to the user's terminal:
/// the session-wide `:worker-output` toggle is on, OR this specific worker is the
/// one currently `:attach`ed to (so `:attach <id>` streams exactly one
/// coordinator without flipping the global toggle). Pure → unit-testable without
/// a live pipe.
fn should_forward(show_output: bool, attached: Option<&str>, label: &str) -> bool {
    show_output || attached == Some(label)
}

/// Braille frames for the `:output` pane's animated "thinking…" indicator — the
/// SAME cycle the interactive engine spinner uses (`engine::Spinner`), so a
/// coordinator's model-reasoning phase reads identically in the pane and in an
/// interactive turn.
const THINKING_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// True when the parent's stderr (where pane rows are drawn) is a terminal — the
/// gate for the animated thinking spinner. A piped/non-TTY parent (tests, the
/// goal loop under `-c`) gets no animation; the static transcript still records
/// the notice. Mirrors `engine::stderr_is_tty`.
fn stderr_is_tty() -> bool {
    // SAFETY: plain isatty query on fd 2.
    unsafe { libc::isatty(2) == 1 }
}

/// True when a raw coordinator-stderr line is the model-reasoning notice
/// (`💭 thinking…`, emitted once per round by `engine::emit_thinking`). The
/// parent renders this as an ANIMATED thinking row in the `:output` pane —
/// matching the interactive `Spinner` — instead of a static row. Pure.
fn is_thinking_notice(raw: &str) -> bool {
    strip_sentinel(raw, "💭").is_some()
}

/// A transient, animated "thinking…" row for ONE worker in the `:output` pane —
/// the streaming-pane analogue of the interactive engine's `Spinner`. While the
/// coordinator is in its model-reasoning phase (it emitted a `💭 thinking…`
/// notice and hasn't produced its next activity line yet) this redraws a cyan
/// braille spinner beside dim "thinking…" IN PLACE on the pane's current row,
/// framed as a normal pane row (border + `[label]` gutter) so it lines up under
/// the stream. It is erased and replaced by the worker's next forwarded line —
/// exactly like the interactive thinking indicator, which animates and then
/// vanishes when output begins. TTY-gated: `start` returns `None` off a terminal,
/// and the caller then falls back to a one-shot static row.
struct ThinkingSpinner {
    /// Registry id — lets [`quiesce_thinking_spinners`] abort exactly this
    /// spinner's task and lets `stop`/natural-exit unregister itself.
    id: u64,
    task: tokio::task::JoinHandle<()>,
}

/// Monotonic id source for live thinking spinners.
static SPINNER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Abort handles for every currently-animating thinking spinner, keyed by id.
/// The REPL drains this synchronously via [`quiesce_thinking_spinners`] on a
/// Shift-Tab cycle/detach so a spinner's async self-erase can't land on — and
/// wipe — the interactive prompt the REPL is about to redraw.
static ACTIVE_SPINNERS: Mutex<Vec<(u64, tokio::task::AbortHandle)>> = Mutex::new(Vec::new());

fn register_spinner(id: u64, handle: tokio::task::AbortHandle) {
    ACTIVE_SPINNERS.lock().unwrap().push((id, handle));
}

fn unregister_spinner(id: u64) {
    if let Ok(mut g) = ACTIVE_SPINNERS.lock() {
        g.retain(|(i, _)| *i != id);
    }
}

/// Synchronously stop every in-flight worker "thinking…" spinner and restore the
/// cursor, then clear the current line. Called by the REPL immediately after a
/// Shift-Tab cycle/detach and BEFORE it redraws the prompt: the spinner tasks
/// poll their forward gate only every ~80 ms, so without this an about-to-close
/// spinner would erase (`\r\x1b[2K`) the freshly-drawn prompt line a beat later —
/// the "prompt doesn't always show after Shift-Tab" bug. Aborting the tasks here
/// guarantees no spinner emits again after the prompt is painted; the `\x1b[?25h`
/// also un-hides the cursor a mid-think spinner had hidden. No-op when idle.
pub fn quiesce_thinking_spinners() {
    let handles: Vec<(u64, tokio::task::AbortHandle)> =
        std::mem::take(&mut *ACTIVE_SPINNERS.lock().unwrap());
    if handles.is_empty() {
        return;
    }
    for (_, h) in handles {
        h.abort();
    }
    if stderr_is_tty() {
        eprint!("\r\x1b[2K\x1b[?25h");
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}

impl ThinkingSpinner {
    /// Start animating a thinking row for `label`, or `None` when stderr isn't a
    /// TTY (no animation possible — the caller prints the static notice instead).
    ///
    /// `show_output` / `attached` are the SAME shared handles the stream loop
    /// gates forwarding on (`should_forward`). The animation re-checks that gate
    /// every frame and self-erases the instant it closes — so when the user
    /// Shift-Tabs to cycle/detach away from a coordinator that is mid-think
    /// (`cycle_worker` mutates `attached`), or flips `:output off`, the thinking
    /// animation is REMOVED promptly instead of spinning on, unwatched, until the
    /// coordinator's next stderr line happens to arrive (which, mid model-reasoning,
    /// can be many seconds off — the loop is parked in `next_line().await`).
    fn start(
        label: &str,
        show_output: Arc<AtomicBool>,
        attached: Arc<Mutex<Option<String>>>,
    ) -> Option<Self> {
        if !stderr_is_tty() {
            return None;
        }
        eprint!("\x1b[?25l"); // hide the cursor while the spinner turns
        let label = label.to_string();
        let id = SPINNER_SEQ.fetch_add(1, Ordering::Relaxed);
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(80));
            for i in 0.. {
                tick.tick().await;
                // Shift-Tab / `:detach` / `:output off` close this worker's
                // forward gate. The stream loop can't react until its next line
                // (it's parked in `next_line().await`), so the spinner watches
                // the gate itself: the moment it closes, erase the row, restore
                // the cursor, and stop — the thinking animation vanishes at once.
                let attached_id = attached.lock().ok().and_then(|g| g.clone());
                if !should_forward(
                    show_output.load(Ordering::Relaxed),
                    attached_id.as_deref(),
                    &label,
                ) {
                    eprint!("\r\x1b[2K\x1b[?25h");
                    break;
                }
                // Cyan braille frame + dim "thinking…", mirroring the interactive
                // spinner's look, framed as a pane row so it carries the same
                // border + `[label]` gutter as every other streamed line.
                // NARRATION_ALIGN_PAD lands the braille frame in the same
                // column as the 🚀 rocket / tool glyph so the animated thinking
                // row aligns with every other streamed glyph (rocket alignment).
                let body = format!(
                    "{NARRATION_ALIGN_PAD}\x1b[36m{}\x1b[0m \x1b[2;36mthinking…\x1b[0m",
                    THINKING_FRAMES[i % THINKING_FRAMES.len()]
                );
                // Green wall for the LIVE post-attach thinking row (this worker
                // is the one `:attach`ed to), cyan for the global `:output` pane —
                // matching the border the forwarded activity rows use.
                let live = attached_id.as_deref() == Some(&label);
                let row = if live {
                    pane_row_live(&label, &body)
                } else {
                    pane_row(&label, &body)
                };
                eprint!("\r\x1b[2K{}", row);
            }
            // Natural exit (gate closed): drop our registry entry so
            // `quiesce_thinking_spinners` doesn't abort an already-finished task.
            unregister_spinner(id);
        });
        register_spinner(id, task.abort_handle());
        Some(Self { id, task })
    }

    /// Stop animating and erase the spinner row, restoring the cursor. The next
    /// pane print (which leads with `\r\x1b[2K`) writes its row onto the cleared
    /// line, so the transient thinking row leaves no trace — mirroring the
    /// interactive spinner being replaced by output.
    fn stop(self) {
        self.task.abort();
        unregister_spinner(self.id);
        eprint!("\r\x1b[2K\x1b[?25h");
    }
}

async fn stream_stderr<R: tokio::io::AsyncRead + Unpin>(
    r: R,
    label: &str,
    show_output: Arc<AtomicBool>,
    attached: Arc<Mutex<Option<String>>>,
    pulse: Option<Arc<WorkerJob>>,
) -> String {
    let mut lines = BufReader::new(r).lines();
    let mut tail: VecDeque<String> = VecDeque::with_capacity(STDERR_TAIL_LINES);
    // The in-flight animated "thinking…" row for this worker, if any. It is
    // started when a `💭 thinking…` notice is forwarded and torn down (erased)
    // the moment the next forwarded line replaces it — exactly like the
    // interactive thinking spinner that animates then vanishes when output begins.
    let mut thinking: Option<ThinkingSpinner> = None;
    while let Ok(Some(line)) = lines.next_line().await {
        // A `message_console` note (📣) is the coordinator's ONE always-surfaced
        // channel: it reaches the operator's terminal IMMEDIATELY, bypassing the
        // `:worker-output` suppression gate entirely. Match it FIRST, before the
        // normal forward/suppress logic. Tear down any in-flight thinking spinner
        // so the note lands on a clean line, print it with its distinct console
        // framing, retain it in the failure tail, and move on — it is never
        // routed through `forward_decision` (which would drop it when output is off).
        if let Some(note) = console_message(&line) {
            if let Some(spin) = thinking.take() {
                spin.stop();
            }
            // Wrap the always-surfaced console note in blank lines so it is set
            // visually apart from whatever printed immediately before it (a
            // coordinator `·result`, a tool row, or the prompt) AND from whatever
            // follows it — rather than abutting either. `announce_raw` appends a
            // single trailing newline (ending the note's own line), so the extra
            // leading `\n` gives the blank line ABOVE and the extra trailing `\n`
            // gives the blank line BELOW.
            crate::tools::announce_raw(&format!("\n{}\n", console_row(label, &note)));
            if tail.len() == STDERR_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
            continue;
        }
        // Drive the prompt-badge pulse from EVERY line (independent of the
        // `:worker-output` forwarding gate) so the badge colour-pulses even when
        // the verbose stream is suppressed — the badge is the quiet liveness cue.
        if let Some(job) = &pulse {
            match classify_event(&line) {
                Some(Pulse::ToolOk) => job.record_tool_outcome(true),
                Some(Pulse::ToolErr) => job.record_tool_outcome(false),
                Some(Pulse::Turn) => job.record_turn_completion(),
                None => {}
            }
        }
        // Capture the forwardable shape of this line (computed as if output
        // were ON) so we can BOTH record it into this worker's transcript —
        // for an `:attach` to replay the output-to-date — AND forward it live
        // when the gate is open. Recording is independent of the gate, so a
        // later `:attach` still shows the history even if `:worker-output`
        // was off the whole time. The single source glyph is already stamped
        // into `text` (engine::tool_glyph / the 🚀/🐌 narration prefix), so the
        // transcript suffix is empty and the pane gutter is just `[label]`.
        if let Some(text) = forward_decision(&line, true) {
            if let Some(job) = &pulse {
                job.record_activity("", &text);
            }
            // Forward when the session-wide toggle is on OR this worker is the
            // one `:attach`ed to (the per-worker `:attach` stream).
            let attached_id = attached.lock().ok().and_then(|g| g.clone());
            let on = should_forward(
                show_output.load(Ordering::Relaxed),
                attached_id.as_deref(),
                label,
            );
            // This forwarded line is LIVE post-attach output when THIS worker is
            // the one currently `:attach`ed to — render it with the green wall
            // (`pane_row_live`) so it's set apart from the cyan "output to date"
            // replay. A line forwarded only because the global `:worker-output`
            // toggle is on (not attached) keeps the cyan `pane_row`.
            let live = attached_id.as_deref() == Some(label);
            let render_row =
                |t: &str| if live { pane_row_live(label, t) } else { pane_row(label, t) };
            if on {
                // This worker's first forwarded line after an `:attach` replaces
                // any attach-time "thinking…" placeholder spinner — stop + erase
                // it so this row lands on the cleared line (no-op once gone).
                if let Some(job) = &pulse {
                    job.stop_backfill_thinking();
                }
                if is_thinking_notice(&line) {
                    // Model-reasoning phase: replace any prior thinking row, then
                    // animate THIS one in place (transient) until the worker's
                    // next forwarded line lands — matching the interactive
                    // `Spinner` that animates then vanishes when output begins.
                    // Off a TTY there's no animation: fall back to a one-shot
                    // static row so piped parents still see the notice.
                    if let Some(spin) = thinking.take() {
                        spin.stop();
                    }
                    // Hand the spinner the SAME forward-gate handles the stream
                    // loop reads, so it can self-erase the moment the user
                    // Shift-Tabs / detaches away mid-think (see ThinkingSpinner).
                    thinking =
                        ThinkingSpinner::start(label, show_output.clone(), attached.clone());
                    if thinking.is_none() {
                        crate::tools::announce_raw(&render_row(&text));
                    }
                } else {
                    // Any other forwarded line ENDS the thinking phase: stop +
                    // erase the spinner so this row lands on the cleared line,
                    // then print it. `announce_raw` re-clears the line first, so
                    // the transient thinking row leaves no trace.
                    if let Some(spin) = thinking.take() {
                        spin.stop();
                    }
                    // Frame each forwarded line as a row of the contained
                    // `:output` pane (a box-drawing left border + label gutter)
                    // so coordinator activity reads as a bordered column rather
                    // than blending into the user's shell scroll. `announce_raw`
                    // prints the pre-framed row (which carries its own colour).
                    crate::tools::announce_raw(&render_row(&text));
                }
            } else if let Some(spin) = thinking.take() {
                // Forwarding just turned off for this worker (e.g. `:detach`
                // mid-think): stop the animation so it doesn't keep drawing while
                // suppressed.
                spin.stop();
            }
        }
        if tail.len() == STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    // Stream ended — tear down any lingering thinking spinner so it doesn't leave
    // the cursor hidden or a half-drawn frame behind, including an attach-time
    // backfill spinner that never got a first line to replace it.
    if let Some(spin) = thinking.take() {
        spin.stop();
    }
    if let Some(job) = &pulse {
        job.stop_backfill_thinking();
    }
    tail.into_iter().collect::<Vec<_>>().join("\n")
}

/// Default address-space / data cap for a worker child, in MB. Generous enough
/// for a real agentic task but bounded so a runaway can't exhaust host memory.
/// Override with `AISH_WORKER_MEM_MB`.
const DEFAULT_WORKER_MEM_MB: u64 = 4096;

/// Hard floor (MB) on the memory cap we will EVER impose on a worker, no matter
/// how low `AISH_WORKER_MEM_MB` is set. Modern language runtimes reserve a large
/// *virtual* address-space region up front — V8/Node maps a multi-GB
/// "cage"/CodeRange, and Go / the JVM reserve comparably large arenas — and that
/// reservation counts against `RLIMIT_AS` (and, on Linux ≥ 4.7, against
/// `RLIMIT_DATA` too, since it now also bounds anonymous `mmap`). Cap a worker
/// below this and those reservations fail *fatally at startup*: `neonctl` (Node)
/// aborts with "Failed to reserve virtual memory for CodeRange" under a ~1 GB
/// cap, and even `node --version`-class tools die once real work initialises an
/// isolate. Empirically Node needs ≳ 2 GB of address space just to start, so we
/// never apply a tighter limit — otherwise a well-meaning low `AISH_WORKER_MEM_MB`
/// would silently break EVERY Node/Go/JVM tool the coordinator tries to run
/// (e.g. `neonctl`, the reported failure). This is a functional floor, not a
/// policy knob: `0` ("no limit") is still honoured; the floor only raises a
/// positive, too-small value.
const MIN_WORKER_MEM_MB: u64 = 2048;

/// Apply the [`MIN_WORKER_MEM_MB`] floor to a requested worker memory cap. `0`
/// means "no limit" and passes through unchanged; any positive value is raised
/// to at least the floor so a modern runtime (V8/Node, Go, JVM) can still
/// reserve its large virtual address space. Pure integer math (async-signal-safe,
/// so it's callable from the post-fork `pre_exec` child) → unit-tested.
fn effective_worker_mem_mb(mem_mb: u64) -> u64 {
    if mem_mb == 0 {
        0
    } else {
        mem_mb.max(MIN_WORKER_MEM_MB)
    }
}

/// Default PID cap for a worker CONTAINER (AC6), mapped to `--pids-limit`.
/// Bounds fork-bomb blast radius. Override with `AISH_WORKER_PIDS`; 0 = no limit.
const DEFAULT_WORKER_PIDS: u64 = 512;

/// Default CPU-time cap for a worker child, in seconds. A backstop against a
/// runaway busy-loop that the wall-clock timeout might not catch promptly.
/// Override with `AISH_WORKER_CPU_SECS`.
const DEFAULT_WORKER_CPU_SECS: u64 = 3600;

/// Parse a `u64` from `var`, falling back to `default` if unset, empty, or
/// unparseable. A value of `0` is treated as "unset / no limit" by the caller.
fn env_u64(var: &str, default: u64) -> u64 {
    parse_u64_or(std::env::var(var).ok().as_deref(), default)
}

/// Pure parsing core of `env_u64`, split out so it's testable without mutating
/// process-wide env (which is `unsafe` and racy under the test harness's
/// threads). `None`/empty/unparseable → `default`; otherwise the parsed value
/// (including a legitimate `0`, which callers read as "no limit").
fn parse_u64_or(raw: Option<&str>, default: u64) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

/// Build the `tokio::process::Command` that re-execs aish in `--coordinator`
/// mode for a single task. Centralises the args, env, pipes, AND the resource
/// limits applied to the child, so `run_worker` and `run_once` stay in sync.
///
/// Resource limits are applied via a `pre_exec` hook (see `apply_rlimits`):
/// they run in the forked child between fork and exec.
/// The coordinator argv (everything AFTER the `aish` binary) for one task:
/// `-c <task> --coordinator --run-id <id> --backend <b> --model <m>`. Factored
/// out of `worker_command` so it is the SINGLE source of truth shared by BOTH
/// execution vehicles (S9.1): the host path execs it directly via `Command`,
/// and the container path passes the identical vector as the container's command
/// (`container::ContainerSpec.argv`). Keeping one builder guarantees the
/// in-container coordinator is invoked byte-for-byte the same as the host one —
/// only the execution vehicle changes. Full parity: the coordinator runs on the
/// SAME backend/model the interactive session uses (claude/grok), inheriting the
/// relevant credential via the env it's spawned with. Pure → unit-tested.
fn coordinator_argv(spec: &WorkerSpec, task: &str, run_id: &str) -> Vec<String> {
    vec![
        "-c".to_string(),
        task.to_string(),
        "--coordinator".to_string(),
        "--run-id".to_string(),
        run_id.to_string(),
        "--backend".to_string(),
        spec.backend.clone(),
        "--model".to_string(),
        spec.model.clone(),
    ]
}

fn worker_command(spec: &WorkerSpec, task: &str, run_id: &str, cwd: &std::path::Path) -> Command {
    let mut cmd = Command::new(&spec.exe);
    // The coordinator argv is the SINGLE source of truth shared with the
    // container backend (see `coordinator_argv` / `container.rs`): the host path
    // execs it directly, the container path passes it as the container command.
    cmd.args(coordinator_argv(spec, task, run_id))
        // The effective run directory: `spec.cwd` normally, or the isolated
        // worktree path when isolation is on.
        .current_dir(cwd)
        // Nested-coordinator guard: an in-container/in-worker aish must never
        // spawn its own workers (no infinite recursion). The child reads this.
        .env("AISH_COORDINATOR", "1")
        // Tie the work to the LAUNCHING session: the child adopts this id so its
        // durable records attribute to the session that asked for the work.
        .env("AISH_LAUNCH_SESSION_ID", &spec.launch_session_id)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(name) = &spec.launch_session_name {
        cmd.env("AISH_LAUNCH_SESSION_NAME", name);
    }
    // Hand the coordinator the launching terminal's width so its narration
    // (rendered non-TTY, then re-framed by us as pane rows) fits tables to the
    // real width minus the pane gutter instead of an unbounded default. Only set
    // when we actually know the width — off a tty the child keeps its bounded
    // fallback, and pane rows are single-line there anyway.
    let term_cols = pane_cols();
    if term_cols != usize::MAX {
        cmd.env("COLUMNS", term_cols.to_string());
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    // Read the knobs in the PARENT (env::var allocates — not allowed in the
    // post-fork child), then move plain integers into the pre_exec closure.
    let mem_mb = env_u64("AISH_WORKER_MEM_MB", DEFAULT_WORKER_MEM_MB);
    let cpu_secs = env_u64("AISH_WORKER_CPU_SECS", DEFAULT_WORKER_CPU_SECS);

    // SAFETY: `pre_exec` runs in the forked child before `exec`, where only
    // async-signal-safe calls are permitted. `apply_rlimits` performs only
    // `setrlimit` syscalls and integer arithmetic — no allocation, no locks,
    // no panics — so it is safe here. Failures are swallowed (best-effort): a
    // worker that couldn't be capped still runs rather than failing to spawn.
    // `tokio::process::Command::pre_exec` is an inherent method (it mirrors
    // `std::os::unix::process::CommandExt::pre_exec`); no extra trait import.
    unsafe {
        cmd.pre_exec(move || {
            // New session (detach from the controlling terminal) so a SIGHUP when
            // the launching interactive session exits doesn't kill this coordinator
            // mid-write, before it can persist its terminal `coordinator_runs` row.
            // (coordinator-lifecycle bug.) SAFETY: setsid() is async-signal-safe;
            // EPERM (already a group leader) is harmless and ignored.
            libc::setsid();
            apply_rlimits(mem_mb, cpu_secs);
            Ok(())
        });
    }
    cmd
}

/// Apply memory and CPU resource limits to the CURRENT process via
/// `setrlimit`. Intended to run inside a `pre_exec` hook (post-fork, pre-exec),
/// so it must stay async-signal-safe: only `setrlimit` syscalls and integer
/// math, no allocation, no logging, no panics. Every call is best-effort —
/// a failed `setrlimit` is silently ignored so the child still execs.
///
/// macOS caveat: on Linux `RLIMIT_AS` is a hard ceiling on the process's
/// virtual address space, so a memory runaway hits `ENOMEM`/abort well before
/// the kernel OOM-killer or macOS Jetsam steps in. On macOS the relationship
/// between `RLIMIT_AS`/`RLIMIT_DATA` and Jetsam's memory-pressure killer is
/// looser — a process can still be SIGKILLed by Jetsam under system pressure
/// regardless of these limits, and these caps don't perfectly track physical
/// footprint. This is harm-reduction (it bounds the worst single-process
/// runaways and is a real cap), NOT a guarantee against signal-9 on macOS.
fn apply_rlimits(mem_mb: u64, cpu_secs: u64) {
    // Runtime-compat floor (see MIN_WORKER_MEM_MB / effective_worker_mem_mb):
    // capping virtual address space below what a modern runtime needs to reserve
    // its cage (V8/Node CodeRange, Go/JVM arenas) makes those tools abort at
    // startup, so a too-small AISH_WORKER_MEM_MB would silently break every
    // Node-based tool (e.g. neonctl) the coordinator runs. 0 ("no limit") passes
    // through untouched; a positive value is raised to at least the floor.
    let mem_mb = effective_worker_mem_mb(mem_mb);
    // 0 == "no limit" for either knob.
    if mem_mb > 0 {
        // Saturate the byte count so a huge MB value can't wrap around.
        let bytes = mem_mb.saturating_mul(1024 * 1024);
        let lim = libc::rlimit {
            rlim_cur: bytes as libc::rlim_t,
            rlim_max: bytes as libc::rlim_t,
        };
        // Cap address space (virtual memory). Best-effort.
        unsafe {
            libc::setrlimit(libc::RLIMIT_AS, &lim);
        }
        // Also cap the data segment as a second line of defence; on some
        // platforms RLIMIT_DATA bites where RLIMIT_AS doesn't.
        unsafe {
            libc::setrlimit(libc::RLIMIT_DATA, &lim);
        }
    }
    if cpu_secs > 0 {
        let lim = libc::rlimit {
            rlim_cur: cpu_secs as libc::rlim_t,
            rlim_max: cpu_secs as libc::rlim_t,
        };
        // Cap CPU seconds — a runaway loop gets SIGXCPU then SIGKILL.
        unsafe {
            libc::setrlimit(libc::RLIMIT_CPU, &lim);
        }
    }
}

// ---------------------------------------------------------------------------
// Worktree isolation — give a writing/building coordinator its own git worktree
// so parallel coordinators can't clobber each other's tree (the headline bug).
// ---------------------------------------------------------------------------

/// A dedicated git worktree carved off `src` for one worker, on a fresh branch.
/// `path` is where the coordinator runs; `branch` is reported on completion so
/// the parent can review/merge changes (we never auto-merge).
struct Worktree {
    path: PathBuf,
    branch: String,
    /// The source repo the worktree was carved from — used to remove it cleanly.
    src: PathBuf,
    /// Commit sha the worktree branched from. Cleanup compares the worktree's tip
    /// to THIS (not the source's live HEAD): branching off `origin/main` means the
    /// base may differ from the source checkout, so an unchanged worktree's tip
    /// equals `base_sha`, not `git_head(src)`.
    base_sha: String,
}

/// True when `dir` is inside a git working tree. Cheap `git rev-parse` probe;
/// false on any error (not a repo, git missing, …).
pub fn is_git_repo(dir: &std::path::Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Parse a GitHub remote URL into a stable, filesystem-safe `owner--repo` key,
/// or `None` when the URL isn't a parseable GitHub remote. Pure — unit-tested.
/// Handles `https://github.com/owner/repo(.git)`, `git@github.com:owner/repo.git`,
/// and `ssh://git@github.com/owner/repo.git`; strips a trailing `.git`/slash and
/// sanitises the `/` separator to `--` (see `sanitize_repo_key`).
fn repo_key_from_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.splitn(2, '/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    Some(sanitize_repo_key(&format!("{owner}/{repo}")))
}

/// Map an `owner/repo` string to a filesystem- and branch-safe key: `/` → `--`,
/// and any other char outside `[A-Za-z0-9._-]` → `-`. Pure.
fn sanitize_repo_key(s: &str) -> String {
    s.replace('/', "--")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Fallback repo-key for a local-only / non-GitHub repo: the source dir's
/// basename plus a short FNV-1a hash of its absolute path, preserving
/// collision-safety across two checkouts of the same-named repo. Pure given `src`.
fn fallback_repo_key(src: &std::path::Path) -> String {
    let base = src
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in src.to_string_lossy().as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!(
        "{}-{:08x}",
        sanitize_repo_key(&base),
        (hash & 0xffff_ffff) as u32
    )
}

/// Resolve the `{repo-key}` for `src` (IO — reads the git `origin` remote):
/// `owner--repo` from the GitHub remote when parseable, else the
/// basename+shorthash fallback. See `repo_key_from_remote` / `fallback_repo_key`.
fn repo_key(src: &std::path::Path) -> String {
    git_out(src, &["remote", "get-url", "origin"])
        .and_then(|url| repo_key_from_remote(&url))
        .unwrap_or_else(|| fallback_repo_key(src))
}

/// The root under which all worker worktrees live: `$AISH_WORKTREE_DIR` when set
/// and non-empty, else `~/.aish/worktrees`, else (no `$HOME`) a temp fallback.
/// Moving off the OS temp dir is the whole point of ISS-2046 — the OS reaps temp
/// dirs and could silently delete a worker's deliberately-kept (dirty) worktree.
/// It lives under aish's OWN home (`~/.aish`, alongside the DB / skills /
/// `.mcp.json`), NOT `~/.atum` (the atum CLI's config dir) — the latter was a
/// stray port artifact that polluted an unrelated tool's directory.
fn worktree_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("AISH_WORKTREE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() {
        return PathBuf::from(home).join(".aish").join("worktrees");
    }
    std::env::temp_dir().join("aish-worktrees")
}

/// Create `dir` (and parents) and tighten it to `0700` — these worktrees can
/// hold un-pushed work, so they're owner-only. Best-effort.
fn ensure_dir_0700(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if std::fs::create_dir_all(dir).is_ok() {
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
}

/// Build the branch name + worktree path for a worker. Pure (the caller supplies
/// the already-resolved `root` and `repo_key`), so it's unit-testable. The
/// worktree lives at `{root}/{repo_key}/{id}` — OUTSIDE the source repo, so it
/// never pollutes the source `git status` — and the branch is `aish/{id}`. The
/// worker id is globally unique (`w_########`, #86), so no session prefix is
/// needed to disambiguate two sessions / two checkouts: they harmlessly share the
/// `{repo_key}` parent dir with distinct leaves.
fn worktree_layout(root: &std::path::Path, repo_key: &str, id: &str) -> (String, PathBuf) {
    let branch = format!("aish/{id}");
    let path = root.join(repo_key).join(id);
    (branch, path)
}

/// Run a git command in `src`, returning trimmed stdout on success.
fn git_out(src: &std::path::Path, args: &[&str]) -> Option<String> {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(src)
        .args(args)
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// True when a git command in `src` exits 0 (output discarded).
fn git_ok(src: &std::path::Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(src)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The repo's trunk branch name — `origin/HEAD` when set (e.g. `main`/`master`),
/// else whichever of `main`/`master` exists locally or on the remote, else `main`.
fn trunk_branch(src: &std::path::Path) -> String {
    if let Some(s) = git_out(
        src,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        if let Some(name) = s.strip_prefix("origin/") {
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    for cand in ["main", "master"] {
        if git_ok(
            src,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{cand}"),
            ],
        ) || git_ok(
            src,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/remotes/origin/{cand}"),
            ],
        ) {
            return cand.to_string();
        }
    }
    "main".to_string()
}

/// Resolve the start-point ref an isolated worker should branch from.
/// `"head"` → the session's current checkout (continue-my-work). Anything else →
/// a clean trunk baseline: `origin/<trunk>` after a best-effort fetch when a
/// remote exists (so workers never inherit a stale local trunk — the exact
/// footgun behind branch sprawl), else the local trunk, else `HEAD`.
fn resolve_base_ref(src: &std::path::Path, base: &str) -> String {
    if base.eq_ignore_ascii_case("head") {
        return "HEAD".to_string();
    }
    let trunk = trunk_branch(src);
    if git_ok(src, &["remote", "get-url", "origin"]) {
        // Refresh so the baseline is genuinely current; ignore failure (offline).
        let _ = git_ok(src, &["fetch", "origin", &trunk]);
        let remote_ref = format!("origin/{trunk}");
        if git_ok(src, &["rev-parse", "--verify", "--quiet", &remote_ref]) {
            return remote_ref;
        }
    }
    if git_ok(
        src,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{trunk}"),
        ],
    ) {
        return trunk;
    }
    "HEAD".to_string()
}

/// Max attempts for `git worktree add` (1 initial try + 4 retries). A transient
/// lock collision on the source repo's `.git` — a CONCURRENT coordinator's
/// `worktree add`/`remove` holding the lock — clears within tens of ms, so a
/// short bounded retry beats silently degrading to the shared cwd.
const WORKTREE_ADD_MAX_ATTEMPTS: u32 = 5;

/// Exponential backoff before retry `n` (1-indexed): `10·2^(n-1)` ms →
/// 10, 20, 40, 80, 160 ms for n = 1..=5. Pure → unit-tested. No new dependency:
/// just `Duration` (already imported) + integer math.
fn worktree_add_backoff(retry: u32) -> Duration {
    // Saturating shift keeps a pathological `retry` from overflowing/panicking.
    let shift = retry.saturating_sub(1).min(20);
    let ms = 10u64.saturating_mul(1u64 << shift);
    Duration::from_millis(ms)
}

/// Whether a `git worktree add` stderr looks like a transient LOCK collision
/// (another live agent / worktree holding the repo lock) rather than a fatal
/// error (bad ref, branch already exists, …). Used only to enrich the retry log
/// — the retry itself is attempted on any failure, a lock being the
/// overwhelmingly common transient cause here. Pure → unit-tested.
fn is_worktree_lock_error(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("another agent is live")
        || s.contains("is already used by worktree")
        || s.contains("cannot lock")
        || s.contains("unable to lock")
        || s.contains(".lock")
}

/// Drive an operation up to `max` attempts with exponential backoff between
/// tries (see `worktree_add_backoff`), sleeping via the injected `sleep`.
/// Returns `true` on the first success, `false` once all attempts are spent. No
/// backoff is taken after the final attempt. The attempt closure and sleeper are
/// injected so the retry policy is unit-testable without spawning git or really
/// sleeping. Pure given its closures.
fn retry_with_backoff<F, S>(max: u32, mut attempt: F, mut sleep: S) -> bool
where
    F: FnMut(u32) -> bool,
    S: FnMut(Duration),
{
    for n in 0..max {
        if attempt(n) {
            return true;
        }
        if n + 1 < max {
            sleep(worktree_add_backoff(n + 1));
        }
    }
    false
}

/// Create a fresh worktree for worker `id`, branched from `base` (`"main"` for a
/// clean trunk baseline, `"head"` to continue the current checkout — see
/// `resolve_base_ref`). Best-effort: returns `None` (caller falls back to the
/// shared `src` cwd) if `src` isn't a repo or `git worktree add` fails.
///
/// Retry strategy: `git worktree add` can fail transiently when a CONCURRENT
/// coordinator holds the source repo's lock (the "another agent is live in this
/// same working tree" collision). Silently returning `None` there dropped the
/// worker into the SHARED cwd — defeating the very isolation this provides and
/// letting parallel coordinators clobber each other's tree. So the add is
/// retried up to `WORKTREE_ADD_MAX_ATTEMPTS` (5) times with exponential backoff
/// (10, 20, 40, 80, 160 ms via `worktree_add_backoff`); only after ALL attempts
/// are exhausted do we fall back to the shared cwd. stderr is captured on each
/// failure to log the cause (and distinguish a lock collision from a fatal
/// error). No new dependencies — `Duration` + `std::thread::sleep` only.
fn create_worktree(src: &std::path::Path, id: &str, base: &str) -> Option<Worktree> {
    if !is_git_repo(src) {
        return None;
    }
    let root = worktree_root();
    let key = repo_key(src);
    let (branch, path) = worktree_layout(&root, &key, id);
    // Off the OS temp dir now (ISS-2046), so aish owns cleanup — create the root
    // and the per-repo parent `0700` before `git worktree add` materialises the leaf.
    ensure_dir_0700(&root);
    if let Some(parent) = path.parent() {
        ensure_dir_0700(parent);
    }
    let start_point = resolve_base_ref(src, base);
    // A stale dir from a crashed prior run would make `git worktree add` fail;
    // best-effort clear it first (only an empty/leftover one is expected here).
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(src)
        .args(["worktree", "remove", "--force"])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    // Try `git worktree add`, retrying a transient lock collision with
    // exponential backoff before giving up. stderr is captured per attempt so a
    // failure can be logged (best-effort) and a lock collision distinguished
    // from a fatal error. Only after all attempts are exhausted does the caller
    // fall back to the shared cwd (see this fn's doc comment).
    let mut last_stderr = String::new();
    let added = retry_with_backoff(
        WORKTREE_ADD_MAX_ATTEMPTS,
        |attempt| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(src)
                .args(["worktree", "add", "-b", &branch])
                .arg(&path)
                .arg(&start_point)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output();
            match out {
                Ok(o) if o.status.success() => true,
                Ok(o) => {
                    last_stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    // Best-effort retry log (never panics). The final failure is
                    // reported by the caller alongside the shared-cwd fallback.
                    if attempt + 1 < WORKTREE_ADD_MAX_ATTEMPTS {
                        let kind = if is_worktree_lock_error(&last_stderr) {
                            "lock collision"
                        } else {
                            "error"
                        };
                        eprintln!(
                            "aish: git worktree add for {} failed ({kind}, attempt {}/{}); retrying after backoff: {}",
                            path.display(),
                            attempt + 1,
                            WORKTREE_ADD_MAX_ATTEMPTS,
                            last_stderr,
                        );
                    }
                    false
                }
                Err(e) => {
                    last_stderr = e.to_string();
                    false
                }
            }
        },
        std::thread::sleep,
    );
    if !added {
        // All retries exhausted — fall back to the shared cwd. Log the last
        // cause best-effort so the degradation isn't silent.
        if !last_stderr.is_empty() {
            eprintln!(
                "aish: git worktree add for {} failed after {} attempts; falling back to shared cwd: {}",
                path.display(),
                WORKTREE_ADD_MAX_ATTEMPTS,
                last_stderr,
            );
        }
        return None;
    }
    // Pin the base commit for clean-up accounting (tip == base_sha ⇒ no commits).
    let base_sha = git_head(&path).unwrap_or_default();
    Some(Worktree {
        path,
        branch,
        src: src.to_path_buf(),
        base_sha,
    })
}

/// True when the worktree has neither uncommitted changes nor commits ahead of
/// where it branched (HEAD of `src` at create time). Such a worktree is "no
/// work was done" and can be torn down. Any git error is treated as "has
/// changes" (conservative — never delete work we can't account for).
fn worktree_is_clean(wt: &Worktree) -> bool {
    // Uncommitted/untracked changes?
    let porcelain = std::process::Command::new("git")
        .arg("-C")
        .arg(&wt.path)
        .args(["status", "--porcelain"])
        .output();
    let dirty = match porcelain {
        Ok(o) if o.status.success() => !o.stdout.is_empty(),
        _ => return false, // can't tell → assume dirty, keep it
    };
    if dirty {
        return false;
    }
    // Any commits added? The worktree branched from `base_sha`; if its tip still
    // equals that, no commits were made → clean. (Compared against the recorded
    // base, not the source's live HEAD, since the base may be origin/<trunk>.)
    match git_head(&wt.path) {
        Some(tip) => !wt.base_sha.is_empty() && tip == wt.base_sha,
        None => false, // can't compare → assume work was done, keep it
    }
}

/// The current HEAD commit sha of a repo/worktree, or `None` on error.
fn git_head(dir: &std::path::Path) -> Option<String> {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if o.status.success() {
        Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        None
    }
}

/// Best-effort startup cleanup: drop git's record of worktrees whose dirs are
/// already gone (a crashed isolated worker can leave a dangling registration).
/// A no-op outside a repo. `git worktree prune` only removes missing entries —
/// it never touches a live worktree, so this is always safe to call.
pub fn prune_worktrees(dir: &std::path::Path) {
    if !is_git_repo(dir) {
        return;
    }
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["worktree", "prune"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// How long a CLEAN, orphaned-or-stale worktree may sit under the managed root
/// before the startup sweeper reclaims it. A dirty / commits-ahead worktree is
/// NEVER swept regardless of age. Override with `AISH_WORKTREE_MAX_AGE_DAYS`.
const DEFAULT_WORKTREE_MAX_AGE_DAYS: u64 = 7;

/// Pure sweep policy: should a discovered worktree leaf be reclaimed? NEVER when
/// it has uncommitted changes (`dirty`) or commits ahead of its base (`ahead`) —
/// the same conservative rule as `worktree_is_clean`. A clean, not-ahead leaf is
/// swept when its git registration is gone (`orphaned`) OR it is at least
/// `max_age_days` old. Unit-tested in isolation from any IO.
fn should_sweep(
    dirty: bool,
    ahead: bool,
    orphaned: bool,
    age_days: u64,
    max_age_days: u64,
) -> bool {
    if dirty || ahead {
        return false; // never delete work left for the operator
    }
    orphaned || age_days >= max_age_days
}

/// Whether the worktree at `leaf` has commits on its branch ahead of the repo
/// trunk it can see. Conservative: any git error → `true` (treat as ahead → keep).
fn worktree_commits_ahead(leaf: &std::path::Path) -> bool {
    let trunk = trunk_branch(leaf);
    // Prefer the remote trunk ref (what isolated workers branch from), falling
    // back to the local trunk; if neither resolves we can't compare → keep.
    let base = if git_ok(
        leaf,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/remotes/origin/{trunk}"),
        ],
    ) {
        format!("origin/{trunk}")
    } else if git_ok(
        leaf,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{trunk}"),
        ],
    ) {
        trunk
    } else {
        return true;
    };
    match git_out(leaf, &["rev-list", "--count", &format!("{base}..HEAD")]) {
        Some(n) => n.trim() != "0",
        None => true,
    }
}

/// Age of `path` in whole days from its mtime, or 0 when unknown (a metadata
/// error makes the leaf look "fresh", so the age rule alone won't reclaim it).
fn dir_age_days(path: &std::path::Path) -> u64 {
    let modified = match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return 0,
    };
    match std::time::SystemTime::now().duration_since(modified) {
        Ok(d) => d.as_secs() / 86_400,
        Err(_) => 0,
    }
}

/// The set of absolute worktree paths git currently tracks for `src` (parsed from
/// `git worktree list --porcelain`). A leaf NOT in this set is orphaned
/// (registration gone). Empty on any error.
fn registered_worktrees(src: &std::path::Path) -> std::collections::HashSet<PathBuf> {
    let mut set = std::collections::HashSet::new();
    if let Some(out) = git_out(src, &["worktree", "list", "--porcelain"]) {
        for line in out.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                let path = PathBuf::from(p.trim());
                set.insert(path.canonicalize().unwrap_or(path));
            }
        }
    }
    set
}

/// Best-effort startup sweeper: reclaim orphaned/old CLEAN worktrees under the
/// managed root so they don't accumulate now that the OS no longer GCs them
/// (ISS-2046). Scoped to the current repo's `{repo-key}` subtree — each repo's
/// startup cleans its own. The conservative `should_sweep` rule guarantees a
/// dirty or commits-ahead worktree (work left for the operator) is NEVER removed.
pub fn sweep_worktrees(src: &std::path::Path) {
    if !is_git_repo(src) {
        return;
    }
    let max_age = env_u64("AISH_WORKTREE_MAX_AGE_DAYS", DEFAULT_WORKTREE_MAX_AGE_DAYS);
    let base_dir = worktree_root().join(repo_key(src));
    let entries = match std::fs::read_dir(&base_dir) {
        Ok(e) => e,
        Err(_) => return, // nothing created under this repo-key yet
    };
    let registered = registered_worktrees(src);
    for entry in entries.flatten() {
        let leaf = entry.path();
        if !leaf.is_dir() {
            continue;
        }
        let canon = leaf.canonicalize().unwrap_or_else(|_| leaf.clone());
        let orphaned = !registered.contains(&canon);
        // Probe cleanliness IN the leaf. A git error → can't confirm clean → treat
        // as dirty (keep): never delete work we can't account for.
        let dirty = match git_out(&leaf, &["status", "--porcelain"]) {
            Some(s) => !s.trim().is_empty(),
            None => true,
        };
        let ahead = dirty || worktree_commits_ahead(&leaf);
        let age = dir_age_days(&leaf);
        if !should_sweep(dirty, ahead, orphaned, age, max_age) {
            continue;
        }
        // Reclaim: drop git's registration (if any) + the `aish/<id>` branch, then
        // the dir. `git worktree remove` deletes a REGISTERED leaf's dir; an orphan
        // it won't touch, so remove that ourselves.
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(src)
            .args(["worktree", "remove", "--force"])
            .arg(&leaf)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Some(name) = leaf.file_name().and_then(|s| s.to_str()) {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(src)
                .args(["branch", "-D", &format!("aish/{name}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = std::fs::remove_dir_all(&leaf);
    }
}

/// A worktree leaf that still holds work (uncommitted changes or commits ahead
/// of its base) — a salvage candidate when its `coordinator_runs` row was lost
/// to an early termination. `id` is the worker/run id (the leaf name), `branch`
/// is `aish/<id>`, `path` the absolute worktree dir.
pub struct OrphanWork {
    pub id: String,
    pub branch: String,
    pub path: PathBuf,
}

/// Scan the managed worktree root for THIS repo and return every leaf that still
/// holds work (dirty OR commits-ahead of its base). The coordinator's salvage
/// pass cross-references these against the durable store to recover runs whose
/// row was lost on early termination (coordinator-lifecycle bug). Best-effort —
/// empty on any error; non-git / unreadable leaves are skipped.
pub fn work_bearing_worktrees(src: &std::path::Path) -> Vec<OrphanWork> {
    if !is_git_repo(src) {
        return Vec::new();
    }
    let base_dir = worktree_root().join(repo_key(src));
    let entries = match std::fs::read_dir(&base_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let leaf = entry.path();
        if !leaf.is_dir() || !is_git_repo(&leaf) {
            continue;
        }
        let dirty = match git_out(&leaf, &["status", "--porcelain"]) {
            Some(s) => !s.trim().is_empty(),
            None => continue, // can't read it as a worktree → skip
        };
        let ahead = worktree_commits_ahead(&leaf);
        if !(dirty || ahead) {
            continue;
        }
        if let Some(name) = leaf.file_name().and_then(|s| s.to_str()) {
            out.push(OrphanWork {
                id: name.to_string(),
                branch: format!("aish/{name}"),
                path: leaf.clone(),
            });
        }
    }
    out
}

/// Remove a worktree and delete its branch — used when the worker made no
/// changes, so nothing is left behind. Best-effort.
fn remove_worktree(wt: &Worktree) {
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(&wt.src)
        .args(["worktree", "remove", "--force"])
        .arg(&wt.path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(&wt.src)
        .args(["branch", "-D", &wt.branch])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Turn a finished child's `ExitStatus` into a human failure note. SIGKILL
/// (signal 9) on these workers is overwhelmingly the OS memory-pressure killer
/// (macOS Jetsam / Linux OOM), so we name that explicitly rather than emit the
/// useless "signal: 9 (SIGKILL)".
fn describe_failure(status: std::process::ExitStatus, role: &str, stderr: &str) -> String {
    use std::os::unix::process::ExitStatusExt;
    if status.signal() == Some(libc::SIGKILL) {
        return format!(
            "{role} was killed by the OS (signal 9) — most likely out of memory; \
             the task may have been too large, or raise AISH_WORKER_MEM_MB. {}",
            stderr.trim()
        )
        .trim_end()
        .to_string();
    }
    format!("{role} exited unsuccessfully ({status}): {}", stderr.trim())
}

/// Pick the coordinator model for a given backend kind. Claude coordinators keep
/// the session's batch model (`batch_model`, Opus by default — deferred work gets
/// the strongest model). Grok has no Batches API and no model tiers worth
/// distinguishing here, so its coordinators run on the Grok default. Anything
/// else falls back to `batch_model`.
pub fn coordinator_model(backend_kind: &str, batch_model: &str) -> String {
    match backend_kind {
        "grok" => crate::backend::grok::DEFAULT_MODEL.to_string(),
        _ => batch_model.to_string(),
    }
}

/// A background worker subprocess, tracked for the life of the session. Shared
/// between the REPL (which lists/surfaces it) and the run task (which mutates it).
pub struct WorkerJob {
    /// Session-local handle, e.g. "w_a7k3m2pQ" (short `w_########` form; legacy
    /// `worker_<uuid>` ids still display and match fine — it's an opaque string).
    pub id: String,
    pub task: String,
    inner: Mutex<JobInner>,
}

struct JobInner {
    /// "running" | "done" | "failed".
    status: String,
    /// OS process id of the currently-running coordinator subprocess (the child
    /// `aish --coordinator`, which `setsid()`s so its pgid == this pid). `Some`
    /// only while a run task has a live child; set right after spawn in
    /// `run_worker` and re-set on each in-place resume. Read by the REPL to
    /// forward a Ctrl-C (SIGINT) to an `:attach`ed worker's process group.
    pid: Option<u32>,
    result: Option<String>,
    error: Option<String>,
    /// Whether this job's result was already surfaced, so the flush doesn't
    /// print it twice.
    displayed: bool,
    /// The git branch this isolated worker left its changes on, if it kept a
    /// worktree (it made changes). Surfaced in the completion notice so the
    /// parent knows where to review/merge. `None` for shared-cwd or no-change runs.
    branch: Option<String>,
    /// Most recent tool-call outcome parsed from this worker's stderr, for the
    /// prompt-badge pulse: `(is_success, when)`. `None` until the first tool
    /// finishes. Read by [`WorkerJob::latest_pulse`] and faded after [`PULSE_FADE`].
    last_tool_outcome: Option<(bool, Instant)>,
    /// When the worker most recently emitted turn/narration text, for the
    /// magenta turn pulse. `None` until the first narration line.
    last_turn_completion: Option<Instant>,
    /// Bounded transcript of forwardable activity lines (`(suffix, text)`)
    /// captured from this worker's stderr REGARDLESS of the `:worker-output`
    /// gate, so an `:attach` can replay the output-to-date before the live
    /// stream resumes. Evicted oldest-first past the line/byte caps.
    transcript: VecDeque<(String, String)>,
    /// Running byte total of `transcript` entries, for the byte-budget cap.
    transcript_bytes: usize,
    /// Wall-clock start (worker construction) — the base for the `:workers`
    /// Started / Runtime columns. Stored as `SystemTime` (not `Instant`) so it
    /// can render an absolute "started X ago" and reconcile with the durable
    /// `coordinator_runs.created_at` timestamps for the same row scheme.
    started: SystemTime,
    /// Wall-clock completion, stamped once by `set_done`/`set_failed`. `None`
    /// while the worker is still running, so the Runtime column shows
    /// elapsed-so-far for a live worker and the frozen total for a terminal one.
    finished: Option<SystemTime>,
    /// Animated "thinking…" spinner shown when an `:attach` lands BEFORE the
    /// coordinator has produced any forwardable activity (empty transcript) —
    /// the attach-pane analogue of the live-stream / interactive spinner, in
    /// place of the old static "no activity captured yet" placeholder row. Owned
    /// here so the live stderr stream can stop it (`stop_backfill_thinking`) the
    /// instant its first forwarded line replaces it. `None` whenever no such
    /// attach-time spinner is active.
    backfill_spinner: Option<ThinkingSpinner>,
    /// How many times this worker has been resumed IN PLACE (operator typed a
    /// follow-up while attached to a finished run). Starts at 0 for the original
    /// run; each `resume_in_place` mints a fresh underlying coordinator `run_id`
    /// (a new "thread" — its own durable `coordinator_runs` row) and bumps this,
    /// while the worker keeps its stable visible `id`, `:workers` slot, and attach
    /// binding. This is what makes "type a message to a finished worker" continue
    /// the SAME worker instead of spawning a brand-new one each time.
    resumes: u32,
}

pub type WorkerJobs = Arc<Mutex<Vec<Arc<WorkerJob>>>>;

/// Everything the spawned run task needs, captured up front so it's
/// self-contained (mirrors how `batch::spawn` captures api_key/model).
pub struct WorkerSpec {
    /// The aish binary to re-exec (this process's own executable).
    pub exe: PathBuf,
    /// Which backend the coordinator child runs on (`"claude"`/`"grok"`). Set
    /// from the active session's `backend_kind` so background work runs on the
    /// same provider as the interactive session (full parity). Threaded into the
    /// child's `--backend` arg by `worker_command`.
    pub backend: String,
    /// Working directory for the child — the session's cwd, so it sees the same
    /// project files and the same project `.mcp.json`. When `isolate` is set this
    /// is the SOURCE repo; the child actually runs in a dedicated worktree carved
    /// off it (see `run_worker`), not in `cwd` itself.
    pub cwd: PathBuf,
    /// Model the child's coordinator turn runs on (Opus by default, like batches).
    pub model: String,
    /// Extra env for the child (the session's `~/.aishrc` exports), so MCP
    /// `${VAR}` interpolation resolves the same as it does here. The child also
    /// inherits the parent's process env (ANTHROPIC_API_KEY, ATUM_*, …).
    pub env: Vec<(String, String)>,
    /// When true and `cwd` is a git repo, run the coordinator in a dedicated
    /// `git worktree` (fresh branch off HEAD) instead of sharing `cwd`, so
    /// parallel coordinators that write/build can't clobber each other's tree.
    /// A no-change worktree is removed on completion; one with changes is left
    /// intact and its branch is surfaced in the result. Set by the model via the
    /// `run_in_background` tool's `isolate` flag (smart-defaulted to true inside a
    /// repo). The goal loop and `:dispatch` leave this false (shared cwd).
    pub isolate: bool,
    /// The git ref an isolated worker branches its worktree from. `"main"`
    /// (the default) means a CLEAN trunk baseline — `origin/<trunk>` after a
    /// best-effort fetch when a remote exists, else the local trunk — so a job
    /// never inherits a stale or unrelated local checkout. `"head"` pins to the
    /// session's current `HEAD` for "continue what I'm working on" tasks. Only
    /// consulted when `isolate` is true.
    pub base: String,
    /// The LAUNCHING session's id — the interactive session that spawned this
    /// coordinator. The child adopts it as its own `session.session_id` so every
    /// durable record it writes (its `coordinator_runs` row, any batches it fans
    /// out) is attributed to the session that asked for the work, not to the
    /// child's throwaway uuid. This is what makes `:workers`/`background_status`
    /// recognize a background job as belonging to "you".
    pub launch_session_id: String,
    /// The launching session's friendly name (`:rename`), if it has one — carried
    /// alongside the id purely for display.
    pub launch_session_name: Option<String>,
    /// Shared `:worker-output` toggle from the launching session. The live stderr
    /// stream reads it PER LINE (see `forward_decision`), so flipping it mid-run
    /// starts/stops forwarding this worker's output. It gates ALL forwarded
    /// coordinator output — the `🔧` tool-activity lines AND the turn/batch
    /// narration — so a background job is QUIET by default (only its `⟳N` prompt
    /// pulse and completion notice show) and streams its full activity only when
    /// `:worker-output` is on.
    pub show_output: Arc<AtomicBool>,
    /// Shared "attached coordinator" handle from the launching session
    /// (`:attach`/`:detach`). When it holds THIS worker's id, the live stderr
    /// stream forwards this worker's activity even with `:worker-output` off — so
    /// `:attach <id>` watches exactly one coordinator without flipping the
    /// session-wide toggle. Read per line, like `show_output`.
    pub attached: Arc<Mutex<Option<String>>>,
    /// The LAUNCHING session's durable coordinator store (host `aish.db`), cloned
    /// so `run_worker` can RECONCILE the child's `coordinator_runs` row after the
    /// child exits. The child (`coordinator::drive`) inserts its own row as
    /// `coordinating` and is normally responsible for finalizing it to
    /// `done`/`failed` — but if the child is killed/crashes/hangs AFTER doing the
    /// real work yet BEFORE writing that terminal phase, the row is orphaned in
    /// `coordinating` forever (the "stale worker entry" bug: `:workers` /
    /// `background_status` show it as still coordinating long after its PR merged).
    /// The parent OUTLIVES the child (`child.wait()` returns) and holds the SAME
    /// run id, so once the child is dead it patches any still-non-terminal row from
    /// ground truth (exit status + `result.txt`). `None` for launch paths without a
    /// store (tests, goal `run_once`); reconciliation is then a no-op.
    pub coordinator_store: Option<crate::db::CoordinatorStore>,
}

impl WorkerSpec {
    /// The per-worker state-dir leaf name — the run/worker id, sanitized to a
    /// filesystem-safe token. Kept here so the volume dir and the container name
    /// derive from the same id.
    fn id_for_state(&self, run_id: &str) -> String {
        run_id
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                    ch
                } else {
                    '-'
                }
            })
            .collect()
    }
}

/// Whether a durable `coordinator_runs` phase left behind by a child that has
/// already exited still needs PARENT reconciliation. Terminal phases
/// (`done`/`failed`) are the child's own authoritative verdict and are left
/// untouched; anything else (`coordinating`, `awaiting_batch`, …) is an orphan
/// once the child process is dead and no live writer can advance it — the "stale
/// worker entry" the operator sees in `:workers`/`background_status`. Kept as a
/// tiny pure predicate so the reconciliation policy is unit-testable without
/// spawning a real child.
fn phase_needs_reconcile(phase: &str) -> bool {
    !matches!(phase, "done" | "failed")
}

impl WorkerJob {
    fn set_done(&self, result: String) {
        let mut i = self.inner.lock().unwrap();
        i.status = "done".into();
        i.result = Some(result);
        i.finished.get_or_insert_with(SystemTime::now);
    }
    /// Record the branch an isolated worker left its changes on (kept worktree).
    fn set_branch(&self, branch: String) {
        self.inner.lock().unwrap().branch = Some(branch);
    }
    fn branch(&self) -> Option<String> {
        self.inner.lock().unwrap().branch.clone()
    }
    /// Record a tool-call outcome for the prompt-badge pulse (green on success,
    /// red on failure). Called from the stderr stream as the coordinator reports
    /// each tool finishing.
    fn record_tool_outcome(&self, success: bool) {
        self.inner.lock().unwrap().last_tool_outcome = Some((success, Instant::now()));
    }
    /// Record a turn/narration completion for the magenta turn pulse.
    fn record_turn_completion(&self) {
        self.inner.lock().unwrap().last_turn_completion = Some(Instant::now());
    }
    /// Append one forwardable activity line to this worker's bounded
    /// transcript so an `:attach` can replay the output-to-date before the
    /// live stream continues. Bounded by BOTH a line count and a byte budget
    /// ([`TRANSCRIPT_MAX_LINES`]/[`TRANSCRIPT_MAX_BYTES`]) so a chatty
    /// coordinator can't grow the parent's memory without limit — the oldest
    /// rows are evicted first.
    fn record_activity(&self, suffix: &str, text: &str) {
        let mut i = self.inner.lock().unwrap();
        i.transcript_bytes += suffix.len() + text.len();
        i.transcript
            .push_back((suffix.to_string(), text.to_string()));
        while i.transcript.len() > TRANSCRIPT_MAX_LINES || i.transcript_bytes > TRANSCRIPT_MAX_BYTES
        {
            match i.transcript.pop_front() {
                Some((s, t)) => i.transcript_bytes -= s.len() + t.len(),
                None => break,
            }
        }
    }

    /// The retained transcript rows (`(suffix, text)`, oldest-first) captured
    /// from this worker's activity — replayed on `:attach`.
    pub fn transcript_rows(&self) -> Vec<(String, String)> {
        self.inner
            .lock()
            .unwrap()
            .transcript
            .iter()
            .cloned()
            .collect()
    }
    /// Begin an ANIMATED "thinking…" row for this worker in the attach pane when
    /// an `:attach` lands before the coordinator has produced any forwardable
    /// activity (empty transcript). Mirrors the live-stream thinking spinner: a
    /// cyan braille frame redrawn in place under the pane, self-erasing the moment
    /// the forward gate closes (`:detach` / `:output off`). The live stream stops
    /// it via [`stop_backfill_thinking`] the instant its first line lands. Returns
    /// `true` when the animation started (stderr is a TTY); `false` off a terminal,
    /// so the caller prints the one-shot static notice instead. `show_output` /
    /// `attached` are the SAME shared handles the stream loop gates forwarding on.
    pub fn start_backfill_thinking(
        &self,
        show_output: Arc<AtomicBool>,
        attached: Arc<Mutex<Option<String>>>,
    ) -> bool {
        match ThinkingSpinner::start(&self.id, show_output, attached) {
            Some(spin) => {
                let mut i = self.inner.lock().unwrap();
                // Replace any stale spinner (a single session attaches one worker
                // at a time, so this should already be None).
                if let Some(old) = i.backfill_spinner.take() {
                    old.stop();
                }
                i.backfill_spinner = Some(spin);
                true
            }
            None => false,
        }
    }

    /// Stop + erase any attach-time thinking spinner this worker is animating
    /// (see [`start_backfill_thinking`]). Called by the live stream the instant
    /// its first forwarded line replaces the placeholder, and again at stream end
    /// so a lingering spinner never leaves the cursor hidden. Cheap no-op once the
    /// slot is empty.
    pub fn stop_backfill_thinking(&self) {
        let spin = self.inner.lock().unwrap().backfill_spinner.take();
        if let Some(spin) = spin {
            spin.stop();
        }
    }

    /// The most recent badge-pulse event on this worker (tool outcome vs turn
    /// completion — whichever happened later), paired with when it happened.
    /// `None` when neither has occurred. Recency is judged by the caller against
    /// [`PULSE_FADE`].
    fn latest_pulse(&self) -> Option<(Pulse, Instant)> {
        let i = self.inner.lock().unwrap();
        let tool = i
            .last_tool_outcome
            .map(|(ok, t)| (if ok { Pulse::ToolOk } else { Pulse::ToolErr }, t));
        let turn = i.last_turn_completion.map(|t| (Pulse::Turn, t));
        match (tool, turn) {
            (Some(a), Some(b)) => Some(if a.1 >= b.1 { a } else { b }),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }
    fn set_failed(&self, err: String) {
        let mut i = self.inner.lock().unwrap();
        i.status = "failed".into();
        i.error = Some(err);
        i.finished.get_or_insert_with(SystemTime::now);
    }

    /// The number of distinct coordinator runs (threads) this worker has had: 1
    /// for the original run, +1 per in-place resume. Surfaced in the resume notice
    /// and `:workers` summary so the operator can see a finished worker was
    /// continued in place (a new thread under the covers) rather than respawned.
    pub fn thread_count(&self) -> u32 {
        self.inner.lock().unwrap().resumes + 1
    }

    /// Reset a TERMINAL worker back to `running` for an in-place resume: clear the
    /// prior run's result/error/transcript/pulse/spinner state, restamp `started`,
    /// and bump the resume counter (a fresh "thread"). The worker's stable `id`,
    /// task, and its slot in `:workers` are untouched, so the operator keeps
    /// interacting with the SAME worker. Returns the new thread number
    /// (`resumes + 1`). Caller guarantees the worker is terminal — only a finished
    /// run is ever resumed.
    fn reset_for_resume(&self) -> u32 {
        let mut i = self.inner.lock().unwrap();
        i.resumes = i.resumes.saturating_add(1);
        i.status = "running".into();
        i.result = None;
        i.error = None;
        i.displayed = false;
        i.branch = None;
        i.last_tool_outcome = None;
        i.last_turn_completion = None;
        i.transcript.clear();
        i.transcript_bytes = 0;
        if let Some(spin) = i.backfill_spinner.take() {
            spin.stop();
        }
        i.started = SystemTime::now();
        i.finished = None;
        i.resumes + 1
    }

    /// Epoch seconds the worker started — the base for the `:workers` Started /
    /// Runtime columns. `None` only if the clock is before the Unix epoch.
    pub fn started_epoch(&self) -> Option<i64> {
        self.inner
            .lock()
            .unwrap()
            .started
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs() as i64)
    }

    /// Epoch seconds the worker reached a terminal state, or `None` while it is
    /// still running. Lets the Runtime column freeze at the total run span once
    /// the worker finishes.
    pub fn finished_epoch(&self) -> Option<i64> {
        self.inner
            .lock()
            .unwrap()
            .finished
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
    }
    pub fn status(&self) -> String {
        self.inner.lock().unwrap().status.clone()
    }
    /// Record the OS pid of the live coordinator subprocess (or clear it with
    /// `None`). Called right after spawn in `run_worker`, and again on each
    /// in-place resume as a fresh child takes over.
    pub fn set_pid(&self, pid: Option<u32>) {
        self.inner.lock().unwrap().pid = pid;
    }
    /// The pid of the currently-running coordinator subprocess, if any. `None`
    /// once the run has finished (or before the child has spawned). Used by the
    /// REPL to forward Ctrl-C (SIGINT) to an `:attach`ed worker's process group.
    pub fn pid(&self) -> Option<u32> {
        self.inner.lock().unwrap().pid
    }
    fn is_terminal(&self) -> bool {
        matches!(
            self.inner.lock().unwrap().status.as_str(),
            "done" | "failed"
        )
    }
    fn is_displayed(&self) -> bool {
        self.inner.lock().unwrap().displayed
    }
    fn mark_displayed(&self) {
        self.inner.lock().unwrap().displayed = true;
    }
    /// The rendered result, a failure note, or a still-running status.
    pub fn fetch(&self) -> String {
        let i = self.inner.lock().unwrap();
        match i.status.as_str() {
            "done" => i.result.clone().unwrap_or_else(|| "(empty result)".into()),
            "failed" => format!(
                "worker {} failed: {}",
                self.id,
                i.error.clone().unwrap_or_else(|| "unknown error".into())
            ),
            other => format!("worker {} is still running (status: {other}).", self.id),
        }
    }
    /// One-line result summary for table cells — mirrors `format_result` in tools.rs.
    /// Running jobs return `"—"`; done jobs show `"✓ success"` (or `"✓ #NN"` when
    /// the result text contains a PR reference); failed jobs show `"✗ <reason>"`
    /// truncated to ~40 chars so the table stays readable.
    pub fn result_cell(&self) -> String {
        let i = self.inner.lock().unwrap();
        match i.status.as_str() {
            "done" => {
                let r = i.result.as_deref().unwrap_or("");
                if let Some(pr) = r.split_whitespace().find(|s| s.starts_with('#')) {
                    format!("✓ {pr}")
                } else {
                    "✓ success".to_string()
                }
            }
            "failed" => {
                let e = i.error.as_deref().unwrap_or("unknown error");
                let truncated = if e.len() > 40 {
                    format!("{}…", &e[..40])
                } else {
                    e.to_string()
                };
                format!("✗ {truncated}")
            }
            _ => "—".to_string(),
        }
    }
}

/// Mint a fresh worker id in the short, readable `w_########` form: a `w_`
/// prefix plus 8 random base62 characters (a-z, A-Z, 0-9), e.g. `w_a7k3m2pQ`.
///
/// Why base62-8 instead of the old 32-hex-char UUID: these ids exist only to
/// disambiguate the handful of concurrent background workers a single host
/// spawns in a session, and they appear constantly in terminal output, logs,
/// and the `background_status` table — so readability wins. 62^8 ≈ 2.18×10^14
/// (~218 trillion) distinct values give ample collision resistance at those
/// counts (even a few thousand live ids keep collision odds negligible) while
/// being ~4× shorter on screen. The randomness is drawn from a UUIDv4's 122
/// bits, so no extra RNG dependency is pulled in — this is dedup-grade
/// uniqueness, not a security token. The id is an opaque string everywhere it's
/// used (only ever compared / `starts_with`-matched, never parsed), so older
/// `worker_<uuid>`-format ids keep displaying and matching correctly.
fn new_worker_id() -> String {
    const ALPHABET: &[u8; 62] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    // Consume a UUIDv4's 122 random bits, peeling off one base62 digit at a
    // time. The modulo bias against 2^128 is astronomically small at 8 digits.
    let mut n = Uuid::new_v4().as_u128();
    let mut suffix = String::with_capacity(8);
    for _ in 0..8 {
        suffix.push(ALPHABET[(n % 62) as usize] as char);
        n /= 62;
    }
    format!("w_{suffix}")
}

// ---------------------------------------------------------------------------
// Container backend (S9.1) — launch a worker in a rootless container instead of
// a host subprocess. Additive and behind a runtime selector; ANY failure here
// degrades gracefully to the host path (AC9), so a missing image / down daemon
// never blocks a job.
// ---------------------------------------------------------------------------

/// Root for per-worker state volumes (AC4): `$AISH_WORKER_STATE_DIR` when set,
/// else `~/.aish/workers`, else a temp fallback. Each worker mounts
/// `<root>/<id>` at `/aish/state`.
pub(crate) fn worker_state_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("AISH_WORKER_STATE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() {
        return PathBuf::from(home).join(".aish").join("workers");
    }
    std::env::temp_dir().join("aish-workers")
}

/// A built container launch: the ready-to-spawn `run` command plus the 0600
/// env-file to delete once the run finishes (secrets live there, never argv).
struct ContainerLaunch {
    cmd: Command,
    env_file: PathBuf,
}

/// Process-env credential/config vars forwarded into the container via the
/// secret env-file so the in-container coordinator authenticates exactly like
/// the host path (which inherits the parent env). Kept out of argv/labels.
const FORWARDED_SECRET_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "XAI_API_KEY",
];

/// RFC3339-ish UTC timestamp for the `aish.created_at` label, without pulling in
/// a date crate: seconds since the epoch is stable, greppable, and sortable.
fn now_label_ts() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

/// Write `pairs` to a fresh 0600 env-file under the worker's state dir (secrets
/// never go in argv — they'd leak into `ps`/labels). Returns the path. Values
/// are written verbatim as `KEY=VALUE` lines; a value with a newline is skipped
/// defensively (env-file is line-oriented).
fn write_env_file(dir: &std::path::Path, pairs: &[(String, String)]) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(".worker.env");
    let mut f = std::fs::File::create(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    for (k, v) in pairs {
        if k.is_empty() || k.contains('\n') || v.contains('\n') {
            continue;
        }
        writeln!(f, "{k}={v}")?;
    }
    Ok(path)
}

/// Build the container `run` Command for a worker, or `None` to fall back to the
/// host path (AC9). Ensures the version-pinned image exists (build-on-first-use
/// via `make worker-image`), creates the 0700 state dir (AC4), writes the 0600
/// secret env-file, and assembles the launch via `container::run_argv`. Blocking
/// IO (image probe/build, fs) — acceptable here, mirroring the blocking git in
/// `create_worktree`.
fn build_container_command(
    rt: crate::container::Runtime,
    spec: &WorkerSpec,
    task: &str,
    run_id: &str,
    run_cwd: &std::path::Path,
) -> Option<ContainerLaunch> {
    use crate::container as c;

    let tag = c::image_tag(crate::update::current_version());
    // Build-on-first-use: a missing image triggers `make worker-image` in the
    // repo root. A build failure → None (host fallback) with a diagnostic.
    if !c::image_exists(rt, &tag) {
        eprintln!(
            "aish: worker image {tag} not found for {} — building via `make worker-image` (first use)…",
            rt.bin()
        );
        let built = std::process::Command::new("make")
            .arg("worker-image")
            .current_dir(&spec.cwd)
            .env("VERSION", crate::update::current_version())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !built || !c::image_exists(rt, &tag) {
            eprintln!(
                "aish: could not build/find worker image {tag}; falling back to host subprocess."
            );
            return None;
        }
    }

    // Preflight compat probe: the image can exist yet be UNRUNNABLE — e.g. a
    // binary built against a newer glibc than the runtime base ships dies at
    // exec with `GLIBC_x.y not found`. Such an image builds + inspects fine, so
    // the checks above pass; only this `run --version` probe catches it. Degrade
    // to the host subprocess with an actionable message rather than launching a
    // doomed container and surfacing an opaque failed job. (Dockerfile.worker's
    // multi-stage self-build makes this rare, but a stale/hand-injected image
    // still fails safe here.)
    if !c::image_runnable(rt, &tag) {
        eprintln!(
            "aish: worker image {tag} exists but its aish binary won't exec in the \
             container (likely a libc mismatch — the image's binary was built against \
             a newer glibc than the runtime base). Rebuild it with `make worker-image` \
             (the multi-stage Dockerfile.worker self-builds against the runtime's libc). \
             Falling back to host subprocess."
        );
        return None;
    }

    // Per-worker state dir (AC4), 0700.
    let state_dir = worker_state_root().join(&spec.id_for_state(run_id));
    ensure_dir_0700(&state_dir);

    // Secret env-file (0600): the rc exports + forwarded process credentials.
    let mut secret_pairs: Vec<(String, String)> = spec.env.clone();
    for key in FORWARDED_SECRET_ENV {
        if let Ok(val) = std::env::var(key) {
            if !val.is_empty() {
                secret_pairs.push(((*key).to_string(), val));
            }
        }
    }
    let env_file = match write_env_file(&state_dir, &secret_pairs) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "aish: couldn't write worker env-file: {e}; falling back to host subprocess."
            );
            return None;
        }
    };

    // Non-secret control env passed inline.
    let mut env_inline = vec![
        ("AISH_COORDINATOR".to_string(), "1".to_string()),
        (
            "AISH_LAUNCH_SESSION_ID".to_string(),
            spec.launch_session_id.clone(),
        ),
    ];
    if let Some(name) = &spec.launch_session_name {
        env_inline.push(("AISH_LAUNCH_SESSION_NAME".to_string(), name.clone()));
    }

    let repo_key = repo_key(&spec.cwd);
    let cspec = c::ContainerSpec {
        name: c::container_name(&spec.launch_session_id, run_id),
        image: tag,
        argv: coordinator_argv(spec, task, run_id),
        labels: c::worker_labels(
            run_id,
            &spec.launch_session_id,
            &repo_key,
            None,
            &now_label_ts(),
        ),
        state_volume_host: state_dir.clone(),
        state_mount: c::STATE_MOUNT.to_string(),
        work_volume_host: Some(run_cwd.to_path_buf()),
        env_file: Some(env_file.clone()),
        env_inline,
        mem_mb: env_u64("AISH_WORKER_MEM_MB", DEFAULT_WORKER_MEM_MB),
        cpus: std::env::var("AISH_WORKER_CPUS")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok()),
        pids_limit: Some(env_u64("AISH_WORKER_PIDS", DEFAULT_WORKER_PIDS)),
        network: std::env::var("AISH_WORKER_NETWORK")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| c::default_network(rt, std::env::consts::OS).to_string()),
        workdir: "/aish/work".to_string(),
    };

    let mut cmd = Command::new(rt.bin());
    cmd.args(c::run_argv(&cspec))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Some(ContainerLaunch { cmd, env_file })
}

/// Register a new background worker and start its run task. Returns the
/// session-local job id. The spec is captured up front so the spawned task is
/// self-contained.
pub fn spawn(jobs: &WorkerJobs, task: String, spec: WorkerSpec) -> String {
    let mut guard = jobs.lock().unwrap();
    // Short, readable `w_########` id (see `new_worker_id`) — still unique enough
    // that workers from different sessions/repos don't collide or mix in
    // `:workers` listings or the coordinator store, but far easier to read in
    // logs and the status table than the old 32-hex-char UUID.
    let id = new_worker_id();
    let job = Arc::new(WorkerJob {
        id: id.clone(),
        task: task.clone(),
        inner: Mutex::new(JobInner {
            pid: None,
            status: "running".into(),
            resumes: 0,
            result: None,
            error: None,
            displayed: false,
            branch: None,
            last_tool_outcome: None,
            last_turn_completion: None,
            transcript: VecDeque::new(),
            transcript_bytes: 0,
            backfill_spinner: None,
            started: SystemTime::now(),
            finished: None,
        }),
    });
    guard.push(job.clone());
    drop(guard);

    // The original run's coordinator `run_id` IS the worker's visible id. A later
    // in-place resume passes a FRESH run id here (a new thread) while reusing this
    // same `WorkerJob`, so the durable side gets a distinct row per thread without
    // the worker changing its visible identity. See `resume_in_place`.
    tokio::spawn(run_worker(jobs.clone(), job, id.clone(), task, spec));
    id
}

/// Resume a FINISHED worker IN PLACE: reuse the SAME `WorkerJob` — keeping its
/// visible `id`, its slot in `:workers`, and the operator's attach binding — but
/// reset it to `running`, mint a FRESH underlying coordinator `run_id` (a new
/// thread, with its own durable `coordinator_runs` row + per-worker transcript),
/// and relaunch the coordinator child seeded with `task`. This is the fix for
/// "typing a follow-up to a finished worker spawns a brand-new worker each time":
/// the operator now continues the SAME worker, with each resume tracked as a new
/// thread under the covers. Returns `(worker_id, thread_number)`. Caller must
/// ensure the worker is terminal (only finished runs are resumed).
pub fn resume_in_place(jobs: &WorkerJobs, job: Arc<WorkerJob>, task: String, spec: WorkerSpec) -> (String, u32) {
    let thread = job.reset_for_resume();
    // A fresh coordinator run id for this thread — distinct from the worker's
    // visible id, so the child's durable record and per-worker transcript don't
    // collide with the prior thread's.
    let run_id = new_worker_id();
    let id = job.id.clone();
    tokio::spawn(run_worker(jobs.clone(), job, run_id, task, spec));
    (id, thread)
}

/// The run task: re-exec aish in `--coordinator` mode, capture stdout as the
/// result, enforce a timeout, then surface it.
/// Drive one coordinator run to completion against `job`. `run_id` is the
/// coordinator run identity for THIS thread — equal to `job.id` for the original
/// run, but a fresh id for an in-place resume (so the child's worktree leaf,
/// durable record, and per-worker transcript are thread-distinct). All
/// operator-facing labels (`[{}]` announces, `:workers` row) stay keyed on the
/// stable `job.id`.
async fn run_worker(jobs: WorkerJobs, job: Arc<WorkerJob>, run_id: String, task: String, spec: WorkerSpec) {
    // Isolation: a writing/building coordinator gets its own git worktree
    // (branched from `spec.base` — a clean trunk baseline by default, or the
    // current HEAD on request) so parallel coordinators can't clobber the shared
    // tree. Best-effort —
    // if `cwd` isn't a repo or `git worktree add` fails, we fall back to the
    // shared cwd (today's behavior). The worktree is torn down on completion if
    // the job made no changes; otherwise it's left intact and its branch reported.
    let worktree = if spec.isolate {
        // Worker ids are globally unique (`w_########`, #86), so the worktree
        // leaf is just the id: two sessions / two checkouts share the `{repo-key}`
        // parent dir but never collide on the leaf. Lives under `~/.aish/worktrees`
        // (off the OS-reaped temp dir — ISS-2046), swept on startup if abandoned.
        create_worktree(&spec.cwd, &run_id, &spec.base)
    } else {
        None
    };
    let run_cwd = worktree
        .as_ref()
        .map(|w| w.path.clone())
        .unwrap_or_else(|| spec.cwd.clone());

    // Pick the execution vehicle (S9.1): a container backend when one is
    // selected and engaged, else today's host subprocess. ANY container-setup
    // failure degrades to the host path (AC9) so a missing image / down daemon
    // never blocks a job. `none`/unset keep the host path byte-for-byte.
    let selection = crate::container::resolve_selection(
        crate::container::Runtime::parse_selector(
            std::env::var("AISH_WORKER_RUNTIME").ok().as_deref(),
        ),
        crate::container::runtime_on_path(crate::container::Runtime::Podman),
        crate::container::runtime_on_path(crate::container::Runtime::Docker),
    );
    let (mut cmd, env_file_cleanup) = match selection {
        crate::container::Selection::Container(rt) => {
            match build_container_command(rt, &spec, &task, &run_id, &run_cwd) {
                Some(launch) => {
                    crate::tools::announce(
                        &format!("[{}]", job.id),
                        &format!("running in a {} container", rt.bin()),
                    );
                    (launch.cmd, Some(launch.env_file))
                }
                None => (worker_command(&spec, &task, &run_id, &run_cwd), None),
            }
        }
        crate::container::Selection::Host => {
            (worker_command(&spec, &task, &run_id, &run_cwd), None)
        }
    };

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            if let Some(wt) = &worktree {
                remove_worktree(wt);
            }
            job.set_failed(format!("couldn't launch worker subprocess: {e}"));
            on_complete(&jobs, &job);
            return;
        }
    };

    // Record the child's pid so the REPL can forward Ctrl-C (SIGINT) to the
    // worker's process group while `:attach`ed. The child called `setsid()`, so
    // its pgid == pid and `kill(-pid, SIGINT)` reaches the whole worker group.
    job.set_pid(child.id());

    // Drain stdout and stderr concurrently (sequential reads can deadlock if the
    // child fills the other pipe's buffer). stdout is the final answer (capped);
    // stderr is STREAMED live — but forwarding is gated behind `:worker-output`
    // (see `stream_stderr`/`forward_decision`), so by default a background job is
    // quiet: its `🔧` tool-activity isn't echoed. A bounded stderr tail is always
    // retained for the failure message regardless of forwarding.
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let label = job.id.clone();
    let show_output = spec.show_output.clone();
    let attached = spec.attached.clone();
    let pulse_job = job.clone();
    let collect = tokio::spawn(async move {
        tokio::join!(
            read_capped(stdout, CAPTURE_CAP),
            stream_stderr(stderr, &label, show_output, attached, Some(pulse_job))
        )
    });

    let status = match tokio::time::timeout(WORKER_TIMEOUT, child.wait()).await {
        Ok(Ok(s)) => Some(s),
        Ok(Err(e)) => {
            collect.abort();
            job.set_failed(format!("worker process error: {e}"));
            on_complete(&jobs, &job);
            return;
        }
        Err(_) => {
            // Timed out — kill the child, then fall through to report it.
            let _ = child.start_kill();
            let _ = child.wait().await;
            None
        }
    };

    let (out, err) = collect.await.unwrap_or_default();
    // Container path: delete the 0600 secret env-file now the run is over.
    if let Some(ef) = &env_file_cleanup {
        let _ = std::fs::remove_file(ef);
    }
    // Finalize the worktree (if any): a clean one (no changes, no new commits) is
    // removed so nothing is left behind; one with work is kept and its branch
    // surfaced so the parent can review/merge it. Returns the kept branch, if any.
    let kept_branch = finalize_worktree(worktree.as_ref());
    if let Some(branch) = &kept_branch {
        job.set_branch(branch.clone());
    }
    match status {
        Some(s) if s.success() => {
            let t = out.trim();
            let mut result = if t.is_empty() {
                "(no output)".to_string()
            } else {
                t.to_string()
            };
            if let Some(wt) = worktree.as_ref() {
                if let Some(branch) = &kept_branch {
                    result.push_str(&format!(
                        "\n\n(changes left on branch `{branch}` in worktree `{}` — review/merge \
from the parent repo; not auto-merged.)",
                        wt.path.display(),
                    ));
                }
            }
            job.set_done(result);
        }
        Some(s) => job.set_failed(describe_failure(s, "worker", &err)),
        None => job.set_failed(format!(
            "worker timed out after {}s",
            WORKER_TIMEOUT.as_secs()
        )),
    }
    // ── Durable-row reconciliation (stale "coordinating" safety net).
    //
    // The child `coordinator::drive` owns finalizing its own `coordinator_runs`
    // row, but if it dies AFTER completing the work yet BEFORE writing the
    // terminal phase (SIGKILL, panic, container teardown, DB write dropped), the
    // row is orphaned as `coordinating` forever — the reported bug where
    // `:workers`/`background_status` show a run "coordinating" long after its PR
    // merged. The parent has just reaped the child (`child.wait()` returned) and
    // holds the SAME `run_id`, so it is now SAFE (no live writer to race) to patch
    // any row the child left non-terminal. We ONLY touch a still-non-terminal row:
    // when the child DID finalize, its record is authoritative and left untouched.
    if let Some(store) = spec.coordinator_store.as_ref() {
        let non_terminal = match store.result_for_run(&run_id) {
            Ok(Some(r)) => phase_needs_reconcile(&r.phase),
            // No row (goal `run_once`, container DB not host-shared) — nothing to
            // reconcile here; the periodic salvage sweep covers those.
            Ok(None) | Err(_) => false,
        };
        if non_terminal {
            let outcome = match status {
                Some(s) if s.success() => {
                    // Prefer the child's cross-boundary result channel, else the
                    // captured stdout, so the reconciled row still carries an answer.
                    let result = crate::worker_store::read_result(&run_id)
                        .filter(|r| !r.trim().is_empty())
                        .unwrap_or_else(|| {
                            let t = out.trim();
                            if t.is_empty() { "(no output)".to_string() } else { t.to_string() }
                        });
                    store.set_done(&run_id, &result)
                }
                Some(s) => store.set_failed(
                    &run_id,
                    &format!(
                        "{} (durable row reconciled by parent — child exited without finalizing)",
                        describe_failure(s, "worker", &err)
                    ),
                ),
                None => store.set_failed(
                    &run_id,
                    &format!(
                        "worker timed out after {}s (durable row reconciled by parent — child killed before finalizing)",
                        WORKER_TIMEOUT.as_secs()
                    ),
                ),
            };
            match outcome {
                Ok(()) => eprintln!(
                    "\x1b[2maish: reconciled orphaned coordinator row {run_id} (child exited without finalizing its status)\x1b[0m"
                ),
                Err(e) => eprintln!(
                    "\x1b[2maish: failed to reconcile orphaned coordinator row {run_id}: {e}\x1b[0m"
                ),
            }
        }
    }
    on_complete(&jobs, &job);
}

/// Tear down or keep a finished worker's worktree. If it has no changes and no
/// commits ahead, remove it + its branch (nothing left behind) and return
/// `None`. If it has work, leave it intact and return the branch name so the
/// parent can review/merge it (never auto-merged).
fn finalize_worktree(worktree: Option<&Worktree>) -> Option<String> {
    let wt = worktree?;
    if worktree_is_clean(wt) {
        remove_worktree(wt);
        None
    } else {
        Some(wt.branch.clone())
    }
}

/// Run a single coordinator subprocess to completion and return its stdout (the
/// The stderr-stream label the background `:goal` loop runs under. The REPL
/// `:attach goal` flow sets the shared `attached` handle to this exact string
/// so each goal turn streams its activity live (see `should_forward`). Exposed
/// as the single source of truth so the attach sentinel can't drift from it.
pub const GOAL_STREAM_LABEL: &str = "goal";

/// final answer). Unlike `spawn`, it doesn't register a tracked job or
/// auto-deliver — the caller consumes the output. Used by the goal loop for each
/// work step.
pub async fn run_once(spec: &WorkerSpec, task: &str, run_id: &str) -> Result<String, String> {
    // The goal loop never isolates (it iterates in the user's live cwd), so we
    // run in `spec.cwd` directly.
    let mut cmd = worker_command(spec, task, run_id, &spec.cwd);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("couldn't launch goal worker: {e}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // stdout is the result (capped capture, unchanged). stderr is streamed live
    // to the user's terminal — gated behind `:worker-output` like the background
    // worker path — retaining only a bounded tail for the failure message.
    let show_output = spec.show_output.clone();
    let attached = spec.attached.clone();
    let collect = tokio::spawn(async move {
        tokio::join!(
            read_capped(stdout, CAPTURE_CAP),
            stream_stderr(stderr, GOAL_STREAM_LABEL, show_output, attached, None)
        )
    });
    let status = match tokio::time::timeout(WORKER_TIMEOUT, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            collect.abort();
            return Err(format!("goal worker process error: {e}"));
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(format!(
                "goal worker timed out after {}s",
                WORKER_TIMEOUT.as_secs()
            ));
        }
    };
    let (out, err) = collect.await.unwrap_or_default();
    if status.success() {
        Ok(out.trim().to_string())
    } else {
        Err(describe_failure(status, "goal worker", &err))
    }
}

/// Called when one worker finishes. While others run, print a brief progress
/// line; once all have finished, flush every not-yet-shown result at once.
/// Mirrors `batch::on_complete`.
fn on_complete(jobs: &WorkerJobs, finished: &Arc<WorkerJob>) {
    // Interactive REPL: the presenter drains at a pause (see batch::on_complete).
    if crate::present::deferred() {
        return;
    }
    let (all_terminal, remaining) = {
        let g = jobs.lock().unwrap();
        let remaining = g.iter().filter(|j| !j.is_terminal()).count();
        (remaining == 0, remaining)
    };
    if !all_terminal {
        crate::tools::announce(
            &format!("[{}]", finished.id),
            &format!(
                "{} — {remaining} worker(s) still running",
                finished.status()
            ),
        );
        return;
    }
    flush_results(jobs);
}

/// Format every finished-but-not-yet-shown worker result into a display block,
/// marking each shown. Shared by the headless flush and the REPL presenter.
pub fn drain_pending(jobs: &WorkerJobs) -> Vec<String> {
    let pending: Vec<Arc<WorkerJob>> = {
        let g = jobs.lock().unwrap();
        g.iter()
            .filter(|j| j.is_terminal() && !j.is_displayed())
            .cloned()
            .collect()
    };
    pending
        .iter()
        .map(|job| {
            let label = if job.status() == "failed" {
                "failed"
            } else {
                "complete"
            };
            job.mark_displayed();
            format!(
                "\x1b[2m── worker {} {label} ──\x1b[0m\n{}",
                job.id,
                crate::md::render_stdout(job.fetch().trim())
            )
        })
        .collect()
}

/// One-line completion NOTICES for finished-but-unshown workers, marking them
/// shown. The presenter notifies (rather than dumping the full result over the
/// prompt); the user views it with `:result <id>`. Result stays in `fetch`.
pub fn notify_pending(jobs: &WorkerJobs) -> Vec<String> {
    let pending: Vec<Arc<WorkerJob>> = {
        let g = jobs.lock().unwrap();
        g.iter()
            .filter(|j| j.is_terminal() && !j.is_displayed())
            .cloned()
            .collect()
    };
    pending
        .iter()
        .map(|job| {
            let (icon, what) = if job.status() == "failed" {
                ("✗", "failed")
            } else {
                ("✓", "done")
            };
            job.mark_displayed();
            // Surface the branch an isolated worker left changes on, so the parent
            // knows where to review/merge without opening the full result.
            let branch = job
                .branch()
                .map(|b| format!(" · branch `{b}`"))
                .unwrap_or_default();
            format!(
                "\x1b[2m{icon} {} {what} — `:result {}` to view · {}{branch}\x1b[0m",
                job.id,
                job.id,
                crate::batch::one_line(&job.task)
            )
        })
        .collect()
}

/// Count of workers still running — for the prompt's `⟳N` indicator.
pub fn running_count(jobs: &WorkerJobs) -> usize {
    jobs.lock()
        .unwrap()
        .iter()
        .filter(|j| !j.is_terminal())
        .count()
}

/// The most recent still-fresh badge pulse across ALL workers (most-recent
/// wins), or `None` when no worker has had an event within [`PULSE_FADE`]. Drives
/// the colour of the prompt's `⟳N` badge.
pub fn fresh_pulse(jobs: &WorkerJobs) -> Option<Pulse> {
    let now = Instant::now();
    jobs.lock()
        .unwrap()
        .iter()
        .filter_map(|j| j.latest_pulse())
        .filter(|(_, when)| now.saturating_duration_since(*when) < PULSE_FADE)
        .max_by_key(|&(_, when)| when)
        .map(|(p, _)| p)
}

/// Build the prompt's `⟳N` background-jobs badge, coloured by the most recent
/// background-worker event:
///   * green `✓N`   — a tool call just succeeded,
///   * red `✗N`     — a tool call just failed,
///   * magenta `⟳N` — the model just emitted a turn/narration line,
///   * dim `⟳N`     — idle (no recent event, or the pulse has faded).
/// `running` is the TOTAL live background-job count (workers + batches); the
/// badge is empty when nothing is running. `pulse` is [`fresh_pulse`]'s verdict.
/// Pure, so the colour/glyph mapping is unit-testable.
pub fn pulse_badge(running: usize, pulse: Option<Pulse>) -> String {
    if running == 0 {
        return String::new();
    }
    match pulse {
        Some(Pulse::ToolOk) => format!("\x1b[32m✓{running}\x1b[0m "), // green tick
        Some(Pulse::ToolErr) => format!("\x1b[31m✗{running}\x1b[0m "), // red cross
        Some(Pulse::Turn) => format!("\x1b[1;35m⟳{running}\x1b[0m "), // bright magenta
        None => format!("\x1b[2m⟳{running}\x1b[0m "),                 // idle dim
    }
}

/// Headless inline flush (no presenter): print every drained block to stdout.
fn flush_results(jobs: &WorkerJobs) {
    let blocks = drain_pending(jobs);
    if blocks.is_empty() {
        return;
    }
    print!("\r\x1b[2K");
    for b in &blocks {
        println!("{b}");
    }
    use std::io::Write;
    std::io::stdout().flush().ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_only_patches_non_terminal_phases() {
        // Child's own authoritative verdict — parent must NOT overwrite.
        assert!(!phase_needs_reconcile("done"));
        assert!(!phase_needs_reconcile("failed"));
        // Orphaned mid-flight phases the dead child never finalized — the stale
        // "coordinating" row the operator saw for w_zrdvGyJC. Parent reconciles.
        assert!(phase_needs_reconcile("coordinating"));
        assert!(phase_needs_reconcile("awaiting_batch"));
        assert!(phase_needs_reconcile("queued"));
    }

    #[test]
    fn reconcile_finalizes_a_stale_coordinating_row() {
        // End-to-end store-side proof: a child inserts its row as `coordinating`,
        // dies without finalizing, and the parent's reconcile path (guarded by
        // `phase_needs_reconcile`) advances it to `done` from ground truth.
        let dir = std::env::temp_dir().join(format!("aish-reconcile-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let store = crate::db::CoordinatorStore::open(&dir.join("aish.db")).unwrap();
        store.insert("w_stale", "resolve conflict for #444", "sess", None).unwrap();
        let before = store.result_for_run("w_stale").unwrap().unwrap();
        assert_eq!(before.phase, "coordinating");
        assert!(phase_needs_reconcile(&before.phase));
        // Parent reconciles on child exit (success):
        store.set_done("w_stale", "PR #444 merged").unwrap();
        let after = store.result_for_run("w_stale").unwrap().unwrap();
        assert_eq!(after.phase, "done");
        assert!(!phase_needs_reconcile(&after.phase));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_forward_gates_on_toggle_or_attach() {
        // Session-wide `:worker-output` on → always forward, attach irrelevant.
        assert!(should_forward(true, None, "w_aaa"));
        assert!(should_forward(true, Some("w_bbb"), "w_aaa"));
        // Toggle off and not attached to THIS worker → suppressed (quiet default).
        assert!(!should_forward(false, None, "w_aaa"));
        assert!(!should_forward(false, Some("w_bbb"), "w_aaa"));
        // Toggle off but attached to THIS worker → forwarded (the :attach stream).
        assert!(should_forward(false, Some("w_aaa"), "w_aaa"));
    }

    #[test]
    fn shift_tab_away_closes_the_thinking_gate() {
        // The ThinkingSpinner re-evaluates `should_forward` every frame off the
        // SAME shared `attached` handle `cycle_worker` mutates. Pressing Shift-Tab
        // to cycle/detach away from the coordinator being watched flips the gate
        // to closed, which is the spinner's signal to erase itself and stop — so
        // the thinking animation is removed promptly rather than spinning on while
        // the stream loop is still parked in `next_line().await`.
        let label = "w_aaa";
        // Per-worker `:attach` stream (output toggle off): watching THIS worker →
        // gate open, animation runs.
        assert!(should_forward(false, Some(label), label));
        // Shift-Tab cycles the attach cursor to another worker → gate closed for
        // this one → spinner self-erases.
        assert!(!should_forward(false, Some("w_bbb"), label));
        // Shift-Tab one more press detaches back to the interactive prompt
        // (attached = None) → still closed → animation gone.
        assert!(!should_forward(false, None, label));
        // But with the session-wide `:output on`, the user wants every
        // coordinator's reasoning visible, so cycling away does NOT kill it.
        assert!(should_forward(true, None, label));
    }

    #[test]
    fn spawn_assigns_unique_ids() {
        // IDs are now the short `w_########` form — readable, and unique enough
        // that two freshly-minted ids don't collide.
        let id1 = new_worker_id();
        let id2 = new_worker_id();
        assert!(id1.starts_with("w_"));
        assert!(id2.starts_with("w_"));
        assert_eq!(id1.len(), 10); // "w_" + 8 chars
        assert_ne!(id1, id2);
    }

    #[test]
    fn worker_id_format_and_collision_freeness() {
        // AC: every id matches `w_` + exactly 8 [a-zA-Z0-9] chars, and a large
        // batch is collision-free in practice (the readability/entropy tradeoff
        // documented on `new_worker_id`).
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            let id = new_worker_id();
            let suffix = id.strip_prefix("w_").expect("id must carry the w_ prefix");
            assert_eq!(suffix.len(), 8, "suffix must be 8 chars: {id}");
            assert!(
                suffix.chars().all(|c| c.is_ascii_alphanumeric()),
                "suffix must be alphanumeric (a-z, A-Z, 0-9): {id}"
            );
            assert!(
                seen.insert(id.clone()),
                "collision generating 10k ids: {id}"
            );
        }
        assert_eq!(seen.len(), 10_000, "all 10k generated ids must be distinct");
    }

    #[test]
    fn fetch_reports_running_then_done() {
        let job = Arc::new(WorkerJob {
            id: "worker_1".into(),
            task: "scan repo".into(),
            inner: Mutex::new(JobInner {
                status: "running".into(),
                pid: None,
            resumes: 0,
                result: None,
                error: None,
                displayed: false,
                branch: None,
                last_tool_outcome: None,
                last_turn_completion: None,
                transcript: VecDeque::new(),
                transcript_bytes: 0,
            backfill_spinner: None,
            started: SystemTime::now(),
            finished: None,
            }),
        });
        assert!(job.fetch().contains("still running"));
        job.set_done("the answer".into());
        assert_eq!(job.fetch(), "the answer");
    }

    #[test]
    fn failed_worker_reports_error() {
        let job = Arc::new(WorkerJob {
            id: "worker_2".into(),
            task: "x".into(),
            inner: Mutex::new(JobInner {
                status: "running".into(),
                pid: None,
            resumes: 0,
                result: None,
                error: None,
                displayed: false,
                branch: None,
                last_tool_outcome: None,
                last_turn_completion: None,
                transcript: VecDeque::new(),
                transcript_bytes: 0,
            backfill_spinner: None,
            started: SystemTime::now(),
            finished: None,
            }),
        });
        job.set_failed("boom".into());
        assert!(job.fetch().contains("worker_2 failed: boom"));
    }

    #[test]
    fn mem_limit_env_parsing() {
        // Unset → default.
        assert_eq!(
            parse_u64_or(None, DEFAULT_WORKER_MEM_MB),
            DEFAULT_WORKER_MEM_MB
        );
        // Valid override (with surrounding whitespace) parses.
        assert_eq!(parse_u64_or(Some(" 1024 "), DEFAULT_WORKER_MEM_MB), 1024);
        // Garbage → default.
        assert_eq!(
            parse_u64_or(Some("not-a-number"), DEFAULT_WORKER_MEM_MB),
            DEFAULT_WORKER_MEM_MB
        );
        // Empty → default.
        assert_eq!(
            parse_u64_or(Some(""), DEFAULT_WORKER_MEM_MB),
            DEFAULT_WORKER_MEM_MB
        );
        // 0 is a legal "no limit" value and must round-trip, not fall back.
        assert_eq!(parse_u64_or(Some("0"), DEFAULT_WORKER_CPU_SECS), 0);
    }

    #[test]
    fn clean_activity_line_forwards_only_result_lines() {
        // Per tool the coordinator emits a bare START line then a ✓/✗ RESULT
        // line. To avoid logging each tool call twice, only the RESULT line is
        // forwarded; the bare START line is dropped.
        //
        // RESULT line (✓ success) — forwarded.
        assert_eq!(
            clean_activity_line("\x1b[2m  ✓ 🔧 read /etc/hosts\x1b[0m"),
            Some("✓ 🔧 read /etc/hosts".to_string())
        );
        // RESULT line (✗ failure) — forwarded.
        assert_eq!(
            clean_activity_line("\x1b[2m  ✗ 🔧 write x\x1b[0m"),
            Some("✗ 🔧 write x".to_string())
        );
        // RESULT line for a LOCAL tool — now carries the 🛠️ hammer-and-wrench
        // (engine::tool_glyph); still forwarded verbatim.
        assert_eq!(
            clean_activity_line("\x1b[2m  ✓ 🛠️ read /etc/hosts\x1b[0m"),
            Some("✓ 🛠️ read /etc/hosts".to_string())
        );
        // Bare START line for a local tool (🛠️, no ✓/✗) is the duplicate — DROPPED.
        assert_eq!(
            clean_activity_line("\x1b[2m  🛠️ read /etc/hosts\x1b[0m"),
            None
        );
        // RESULT line with the real inner colour codes engine.rs emits — still
        // forwarded; the outer dim wrapper is stripped, inner colour preserved.
        assert_eq!(
            clean_activity_line("\x1b[2m  \x1b[32m✓\x1b[0m 🔧 read /etc/hosts\x1b[0m"),
            Some("\x1b[32m✓\x1b[0m 🔧 read /etc/hosts".to_string())
        );
        // Bare START lines (wrench, no ✓/✗) are the duplicate — DROPPED now.
        assert_eq!(clean_activity_line("\x1b[2m  🔧 git status\x1b[0m"), None);
        assert_eq!(
            clean_activity_line("\x1b[2m  🔧 mcp__atum__list_tools\x1b[0m"),
            None
        );
        assert_eq!(clean_activity_line("🔧 ls"), None);
        // Lines without the wrench are dropped (banner, blanks, prose).
        assert_eq!(clean_activity_line("coordinator run abc starting"), None);
        assert_eq!(clean_activity_line(""), None);
        assert_eq!(clean_activity_line("   \x1b[2m\x1b[0m  "), None);
    }

    #[test]
    fn strip_sentinel_extracts_turn_and_batch_lines() {
        // 🗨 turn text (emitted by the coordinator's engine narration).
        assert_eq!(
            strip_sentinel("🗨 planning the migration", "🗨"),
            Some("planning the migration".to_string())
        );
        // 📦 batch fan-out notice.
        assert_eq!(
            strip_sentinel("📦 fanned 3 sub-task(s) out", "📦"),
            Some("fanned 3 sub-task(s) out".to_string())
        );
        // Wrong sentinel / plain lines / empty payload → None.
        assert_eq!(strip_sentinel("🗨 hi", "📦"), None);
        assert_eq!(strip_sentinel("just prose", "🗨"), None);
        assert_eq!(strip_sentinel("🗨   ", "🗨"), None);
        // A 🔧 tool line is NOT a turn line (it routes through clean_activity_line).
        assert_eq!(strip_sentinel("🔧 git status", "🗨"), None);
    }

    #[test]
    fn forward_decision_gates_all_output_on_worker_output_toggle() {
        // A tool emits a bare START line then a ✓/✗ RESULT line. Only the RESULT
        // line is forwarded (so each tool call logs exactly once).
        let tool_start = "\x1b[2m  🔧 git status\x1b[0m";
        let tool_result = "\x1b[2m  ✓ 🔧 git status\x1b[0m";
        let turn = "🗨 planning the migration";
        let batch = "📦 fanned 3 sub-task(s) out";
        let banner = "coordinator run abc starting";

        // OFF (default): EVERYTHING is suppressed. This is the headline behavior:
        // a background coordinator is quiet by default.
        assert_eq!(forward_decision(tool_start, false), None);
        assert_eq!(forward_decision(tool_result, false), None);
        assert_eq!(forward_decision(turn, false), None);
        assert_eq!(forward_decision(batch, false), None);
        assert_eq!(forward_decision(banner, false), None);

        // ON: the full live stream returns as `[label] …` rows — but the tool
        // call is forwarded ONCE, via its RESULT line only, VERBATIM (the
        // coordinator already stamped the single source glyph).
        assert_eq!(forward_decision(tool_start, true), None); // duplicate START — dropped
        assert_eq!(
            forward_decision(tool_result, true),
            Some("✓ 🔧 git status".to_string())
        );
        assert_eq!(
            forward_decision(turn, true),
            // Rocket prefixes the text (it sits after the [label] gutter).
            Some("  🚀 planning the migration".to_string())
        );
        assert_eq!(
            forward_decision(batch, true),
            Some("  🐌 fanned 3 sub-task(s) out".to_string())
        );
        // Noise (banner/blank) is dropped even when output is ON.
        assert_eq!(forward_decision(banner, true), None);
        assert_eq!(forward_decision("", true), None);
    }

    #[test]
    fn forward_decision_surfaces_thinking_when_output_on() {
        // A coordinator emits a 💭 sentinel when it enters its model-reasoning
        // phase. Like every other coordinator line it is gated behind
        // :worker-output: suppressed by default, and surfaced when the toggle is
        // on with the 💭 glyph padded to the shared "rocket alignment" column
        // (under the 🚀 narration / tool glyph) → `  💭 thinking…`.
        let thinking = "💭 thinking…";
        // OFF (default): suppressed with everything else.
        assert_eq!(forward_decision(thinking, false), None);
        // ON: forwarded with the NARRATION_ALIGN_PAD so 💭 lines up under 🚀.
        assert_eq!(
            forward_decision(thinking, true),
            Some("  💭 thinking…".to_string())
        );
        // The 💭 sentinel is a turn-ish narration, not a 🔧 tool line, so it must
        // NOT be classified as a tool-activity line.
        assert_eq!(clean_activity_line(thinking), None);
    }

    #[test]
    fn is_thinking_notice_detects_the_reasoning_sentinel() {
        // The 💭 notice drives the ANIMATED thinking row in the pane; every other
        // line (tool RESULT, 🗨 narration, 📦 batch, noise) does NOT.
        assert!(is_thinking_notice("💭 thinking…"));
        assert!(is_thinking_notice("  💭 thinking…")); // leading indent tolerated
        assert!(!is_thinking_notice("🗨 planning the migration"));
        assert!(!is_thinking_notice("📦 fanned 3 sub-task(s) out"));
        assert!(!is_thinking_notice("\x1b[2m  ✓ 🔧 read /etc/hosts\x1b[0m"));
        assert!(!is_thinking_notice("💭")); // bare sentinel, no text -> not a notice
        assert!(!is_thinking_notice(""));
    }

    #[test]
    fn worker_output_logs_each_tool_call_exactly_once() {
        // Regression: the duplicate tool-call logging bug. A single tool call
        // produces a START line then a RESULT line on the coordinator's stderr;
        // with :worker-output ON, the forwarder must emit exactly ONE line for
        // that call (the RESULT line), not two.
        let per_call = [
            "\x1b[2m  🔧 mcp__atum__atum_get_project_task\x1b[0m", // START
            "\x1b[2m  ✓ 🔧 mcp__atum__atum_get_project_task\x1b[0m", // RESULT
        ];
        let forwarded: Vec<String> = per_call
            .iter()
            .filter_map(|l| forward_decision(l, true))
            .collect();
        assert_eq!(
            forwarded,
            // The RESULT line is forwarded verbatim (single wrench) — exactly once.
            vec!["✓ 🔧 mcp__atum__atum_get_project_task".to_string()],
            "each tool call must forward exactly once (the RESULT line)"
        );
    }

    #[test]
    fn forward_decision_forwards_result_line_verbatim_when_output_on() {
        // The coordinator already stamped the single source glyph between the
        // status icon and the desc, so forward_decision forwards the cleaned
        // RESULT line VERBATIM — no extra wrench/gear is prepended. OFF → None.
        let local = "\x1b[2m  ✓ 🛠️ read /etc/hosts\x1b[0m";
        let mcp = "\x1b[2m  ✓ 🔧 mcp__atum__atum_get_project_task\x1b[0m";
        assert_eq!(forward_decision(local, false), None);
        assert_eq!(forward_decision(mcp, false), None);
        assert_eq!(
            forward_decision(local, true),
            Some("✓ 🛠️ read /etc/hosts".to_string())
        );
        assert_eq!(
            forward_decision(mcp, true),
            Some("✓ 🔧 mcp__atum__atum_get_project_task".to_string())
        );
    }

    #[test]
    fn pane_row_frames_with_border_label_and_preserved_text() {
        // A pane row carries the cyan box-drawing left border, the dim [label]
        // gutter (just the worker id), then the text VERBATIM (so any inline
        // colour the coordinator emitted survives). This is what visually
        // contains the `:output` stream as a bordered side-column (w_sn1fHhd5).
        let row = pane_row("w_a7k3m2pQ", "🚀 planning the migration");
        assert!(
            row.starts_with(PANE_BORDER),
            "row must open with the pane border: {row}"
        );
        assert!(row.contains("┃"), "border glyph present: {row}");
        assert!(
            row.contains("[w_a7k3m2pQ]"),
            "gutter carries just the worker id: {row}"
        );
        assert!(
            row.ends_with("🚀 planning the migration"),
            "text preserved at the end: {row}"
        );

        // The text's own colour codes are passed through untouched.
        let colored = pane_row("w_a7k3m2pQ", "\x1b[32m✓\x1b[0m 🔧 read /etc/hosts");
        assert!(
            colored.contains("[w_a7k3m2pQ]"),
            "gutter is the bare label: {colored}"
        );
        assert!(
            colored.contains("\x1b[32m✓\x1b[0m 🔧 read /etc/hosts"),
            "inline colour preserved: {colored}"
        );
    }

    #[test]
    fn pane_row_hang_indents_wrapped_message_under_first_letter() {
        let label = "w_abc12345";
        let msg = "the quick brown fox jumps over the lazy dog again and again";
        // Pure core with an explicit narrow width → wraps deterministically.
        // (indent = 15 gutter + 3 for "🚀 " = 18; avail = 60-18 = 42 ≥ MIN_WRAP.)
        let row = pane_row_cols(label, &format!("🚀 {msg}"), 60, PANE_BORDER);

        let lines: Vec<&str> = row.split('\n').collect();
        assert!(lines.len() >= 2, "message should wrap to >=2 rows: {row:?}");

        // Every row carries the cyan border.
        for l in &lines {
            assert!(l.starts_with(PANE_BORDER), "row keeps the border: {l:?}");
        }
        // Opening row: "┃ [w_abc12345] " = 5 + 10 = 15 cols, then "🚀 " (3 cols)
        // → the message starts at column 18. A continuation row is
        // "┃" + 17 spaces so its text begins in that SAME column (the rocket /
        // first-letter alignment the user asked for).
        let cont = &lines[1];
        let after_border = cont.strip_prefix(PANE_BORDER).unwrap();
        let spaces = after_border.chars().take_while(|c| *c == ' ').count();
        assert_eq!(
            spaces, 17,
            "continuation hangs under the first letter: {cont:?}"
        );
        // No row exceeds the terminal width.
        for l in &lines {
            assert!(vis_cols(l) <= 60, "row within width: {l:?} ({})", vis_cols(l));
        }
    }

    #[test]
    fn pane_row_single_line_when_width_unknown() {
        // usize::MAX means "unknown width" → never wrap; long line emitted
        // verbatim (back-compat with the pre-wrap behaviour).
        let long = "x".repeat(500);
        let row = pane_row_cols("w_a7k3m2pQ", &format!("🚀 {long}"), usize::MAX, PANE_BORDER);
        assert!(!row.contains('\n'), "no wrapping when width is unknown");
        assert!(row.ends_with(&long), "text preserved verbatim");
    }

    #[test]
    fn pane_row_ansi_message_not_wrapped() {
        // At a width too narrow to hang-indent (avail < MIN_WRAP_COLS) the row is
        // never wrapped — even an ANSI-bearing message is emitted single-line.
        // ("w_a7k3m2pQ" gutter = 15; cols = 30 ⇒ avail = 15 < MIN_WRAP_COLS.)
        let body = "\x1b[2;36mthinking… a very long status that would otherwise wrap across the terminal width here\x1b[0m";
        let row = pane_row_cols("w_a7k3m2pQ", body, 30, PANE_BORDER);
        assert!(!row.contains('\n'), "too narrow to hang-indent → single line");
    }

    #[test]
    fn pane_row_wraps_ansi_message_preserving_color() {
        // Regression: markdown-rendered narration carries inline SGR (a `code`
        // span → dim). It must WRAP (hang-indented) like plain text — the old
        // code bailed to a single line on any '\x1b', so the terminal soft-wrapped
        // it back to column 0. Each continuation line must re-open the active
        // colour and reset, and stay within the width.
        let label = "w_abc12345"; // gutter = 15
        let msg = "CI gate clean: cargo \x1b[2mtest --no-default-features --locked\x1b[0m — 768 lib + 26 memory + 5 routing pass here";
        let row = pane_row_cols(label, &format!("🚀 {msg}"), 50, PANE_BORDER);

        let lines: Vec<&str> = row.split('\n').collect();
        assert!(lines.len() >= 2, "ANSI message should wrap: {row:?}");
        for l in &lines {
            assert!(l.starts_with(PANE_BORDER), "row keeps the border: {l:?}");
            assert!(vis_cols(l) <= 50, "row within width: {l:?} ({})", vis_cols(l));
        }
        // Continuation hangs under the first message letter: "🚀 " = 3 cols after
        // the 15-col gutter ⇒ indent 18 ⇒ "┃" + 17 spaces.
        let after = lines[1].strip_prefix(PANE_BORDER).unwrap();
        assert_eq!(
            after.chars().take_while(|c| *c == ' ').count(),
            17,
            "continuation hangs under the first letter: {:?}",
            lines[1]
        );
        // Colour survives: the dim open code and a reset are both present.
        assert!(row.contains("\x1b[2m"), "dim code preserved: {row:?}");
        assert!(row.contains("\x1b[0m"), "reset present: {row:?}");
        // Content integrity: stripping ANSI + border/pad reassembles the message.
        let strip_ansi = |s: &str| -> String {
            let mut out = String::new();
            let mut it = s.chars().peekable();
            while let Some(c) = it.next() {
                if c == '\x1b' {
                    for cc in it.by_ref() {
                        if cc.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        };
        let visible: String = lines
            .iter()
            .map(|l| {
                let no_ansi = strip_ansi(l);
                let body = no_ansi.strip_prefix('┃').unwrap_or(&no_ansi).trim_start();
                body.strip_prefix("[w_abc12345]")
                    .map(|s| s.trim_start().to_string())
                    .unwrap_or_else(|| body.to_string())
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            visible.contains("CI gate clean: cargo test --no-default-features --locked")
                && visible.contains("768 lib + 26 memory + 5 routing pass here"),
            "message text intact across wraps: {visible:?}"
        );
    }

    #[test]
    fn vis_cols_counts_vs16_emoji_as_two_columns() {
        // Regression: a base emoji + U+FE0F emoji-presentation selector (🛠️/⚙️/✏️)
        // renders as 2 terminal columns but `UnicodeWidthChar::width` per-char
        // undercounts it to 1 (base 1 + selector 0). String-level width is honest.
        assert_eq!(vis_cols("\u{1F6E0}\u{FE0F}"), 2, "🛠️ is 2 cols");
        assert_eq!(vis_cols("\u{2699}\u{FE0F}"), 2, "⚙️ is 2 cols");
        // The exact tool-result prefix from the bug report: "✓ 🛠️ ".
        assert_eq!(vis_cols("\u{2713} \u{1F6E0}\u{FE0F} "), 5, "✓ 🛠️  prefix is 5 cols");
        // A bare rocket (already width-2, no selector) is unchanged.
        assert_eq!(vis_cols("\u{1F680}"), 2, "🚀 is 2 cols");
    }

    #[test]
    fn step_width_promotes_vs16_pair() {
        // Emoji base followed by the VS16 selector → promoted to 2.
        assert_eq!(step_width('\u{1F6E0}', Some('\u{FE0F}')), 2);
        // Same base with no trailing selector → its intrinsic (narrow) width.
        assert_eq!(step_width('\u{1F6E0}', None), 1);
        // The selector itself contributes nothing (the pair still totals 2).
        assert_eq!(step_width('\u{FE0F}', None), 0);
        // A wide emoji is untouched by a (non-)selector follow char.
        assert_eq!(step_width('\u{1F680}', Some('x')), 2);
    }

    #[test]
    fn pane_row_vs16_emoji_prefix_stays_within_width() {
        // Regression for the reported soft-wrap: a tool-result row whose glyph
        // prefix is `✓ 🛠️ ` (base emoji + VS16) was measured one column short, so
        // the first wrapped row came out `cols + 1` wide and the terminal folded
        // its last character ("g" of "streaming") onto a stray line. Every emitted
        // row must now sit within the terminal width.
        let label = "w_bduJrw6R"; // gutter = 5 + 10 = 15
        let msg = "gh pr create --repo LightHeart-Ventures/aish --base main \
                   --head aish/w_bduJrw6R --title feat streaming completions \
                   through the backend trait implements streaming end to end";
        let text = format!("\u{2713} \u{1F6E0}\u{FE0F} {msg}");
        let cols = 72;
        let row = pane_row_cols(label, &text, cols, PANE_BORDER);
        let lines: Vec<&str> = row.split('\n').collect();
        assert!(lines.len() >= 2, "long tool-result should wrap: {row:?}");
        for l in &lines {
            assert!(
                vis_cols(l) <= cols,
                "row must not exceed width {cols}: {l:?} ({} cols)",
                vis_cols(l)
            );
        }
    }

    #[test]
    fn pane_frames_bracket_the_region() {
        // The open/close frames bracket the pane when `:output` is toggled.
        // Both carry the cyan border-drawing characters and name the pane.
        let open = pane_open();
        let close = pane_close();
        assert!(
            open.contains("┏"),
            "open frame has a top-left corner: {open}"
        );
        assert!(
            open.contains("coordinator output"),
            "open frame names the pane: {open}"
        );
        assert!(
            close.contains("┗"),
            "close frame has a bottom-left corner: {close}"
        );
        assert!(
            close.contains("coordinator output"),
            "close frame names the pane: {close}"
        );
    }

    #[test]
    fn pane_replay_header_brackets_output_to_date() {
        let h = pane_replay_header("w_a7k3m2pQ");
        assert!(
            h.contains('\u{250f}'),
            "replay header has a top-left corner: {h}"
        );
        assert!(
            h.contains("w_a7k3m2pQ"),
            "replay header names the coordinator: {h}"
        );
        assert!(
            h.contains("output to date"),
            "replay header labels the block: {h}"
        );
    }

    #[test]
    fn pane_input_row_sets_apart_the_task_with_emoji_and_bold() {
        // The coordinator's input (its task) opens the replay and must stand out
        // as the START of the conversation: a 💬 glyph + bold, framed as a normal
        // pane row (border + [label] gutter) with the task text preserved.
        let row = pane_input_row("w_a7k3m2pQ", "review the design doc");
        assert!(
            row.starts_with(PANE_BORDER),
            "input row carries the pane border: {row}"
        );
        assert!(
            row.contains("[w_a7k3m2pQ]"),
            "gutter carries the worker id: {row}"
        );
        assert!(row.contains('💬'), "input row carries the speech glyph: {row}");
        assert!(row.contains("\x1b[1m"), "input row is bold: {row}");
        assert!(
            row.contains("review the design doc"),
            "task text preserved: {row}"
        );
        // The bold is closed so it doesn't bleed into following rows.
        assert!(row.ends_with("\x1b[0m"), "bold is reset at the end: {row}");
    }

    #[test]
    fn transcript_records_and_bounds_oldest_first() {
        let job = Arc::new(WorkerJob {
            id: "w_tx".into(),
            task: "t".into(),
            inner: Mutex::new(JobInner {
                status: "running".into(),
                pid: None,
            resumes: 0,
                result: None,
                error: None,
                displayed: false,
                branch: None,
                last_tool_outcome: None,
                last_turn_completion: None,
                transcript: VecDeque::new(),
                transcript_bytes: 0,
            backfill_spinner: None,
            started: SystemTime::now(),
            finished: None,
            }),
        });
        // Record well past the line cap; the oldest rows are evicted.
        for n in 0..(TRANSCRIPT_MAX_LINES + 50) {
            job.record_activity("", &format!("line {n}"));
        }
        let rows = job.transcript_rows();
        assert!(
            rows.len() <= TRANSCRIPT_MAX_LINES,
            "line cap enforced: {}",
            rows.len()
        );
        // The newest line is retained; the very first is gone.
        let newest = format!("line {}", TRANSCRIPT_MAX_LINES + 49);
        assert!(rows.last().unwrap().1.contains(&newest), "newest row kept");
        assert!(
            !rows.iter().any(|(_, t)| t == "line 0"),
            "oldest row evicted"
        );
    }

    #[test]
    fn worktree_layout_builds_branch_and_path() {
        let root = std::path::Path::new("/wt-root");
        let (branch, path) = worktree_layout(root, "LightHeart-Ventures--aish", "w_a7k3m2pQ");
        // Branch is just `aish/{id}` — the id is globally unique, no session prefix.
        assert_eq!(branch, "aish/w_a7k3m2pQ");
        // Path = {root}/{repo-key}/{id}; the leaf is exactly the worker id.
        assert_eq!(
            path,
            root.join("LightHeart-Ventures--aish").join("w_a7k3m2pQ")
        );
        assert!(path.ends_with("w_a7k3m2pQ"), "got: {}", path.display());
        // Distinct ids never collide on path or branch.
        let (b2, p2) = worktree_layout(root, "LightHeart-Ventures--aish", "w_ZZ00ay12");
        assert_ne!(branch, b2);
        assert_ne!(path, p2);
        // Distinct repo-keys get distinct parent dirs even with the same id.
        let (_, other) = worktree_layout(root, "other--repo", "w_a7k3m2pQ");
        assert_ne!(path, other);
    }

    #[test]
    fn repo_key_from_remote_parses_https_and_ssh() {
        // https, with and without the `.git` suffix.
        assert_eq!(
            repo_key_from_remote("https://github.com/LightHeart-Ventures/aish.git").as_deref(),
            Some("LightHeart-Ventures--aish")
        );
        assert_eq!(
            repo_key_from_remote("https://github.com/LightHeart-Ventures/aish").as_deref(),
            Some("LightHeart-Ventures--aish")
        );
        // Trailing slash is stripped.
        assert_eq!(
            repo_key_from_remote("https://github.com/octo/Hello-World/").as_deref(),
            Some("octo--Hello-World")
        );
        // `git@` ssh form.
        assert_eq!(
            repo_key_from_remote("git@github.com:LightHeart-Ventures/aish.git").as_deref(),
            Some("LightHeart-Ventures--aish")
        );
        // `ssh://` form.
        assert_eq!(
            repo_key_from_remote("ssh://git@github.com/owner/repo.git").as_deref(),
            Some("owner--repo")
        );
        // Non-GitHub / unparseable → None (caller falls back to basename+hash).
        assert_eq!(
            repo_key_from_remote("https://gitlab.com/owner/repo.git"),
            None
        );
        assert_eq!(
            repo_key_from_remote("git@bitbucket.org:owner/repo.git"),
            None
        );
        assert_eq!(repo_key_from_remote("not a url"), None);
        assert_eq!(repo_key_from_remote("https://github.com/owner"), None); // no repo segment
    }

    #[test]
    fn sanitize_repo_key_maps_slash_and_unsafe_chars() {
        assert_eq!(sanitize_repo_key("owner/repo"), "owner--repo");
        // Allowed chars (alnum, dot, underscore, hyphen) survive.
        assert_eq!(sanitize_repo_key("Acme.Co/My_Repo-1"), "Acme.Co--My_Repo-1");
        // Anything else collapses to '-'.
        assert_eq!(sanitize_repo_key("a b/c:d"), "a-b--c-d");
    }

    #[test]
    fn fallback_repo_key_is_basename_plus_stable_shorthash() {
        let a = fallback_repo_key(std::path::Path::new("/home/me/projects/aish"));
        let b = fallback_repo_key(std::path::Path::new("/home/me/projects/aish"));
        // Stable for the same absolute path.
        assert_eq!(a, b);
        assert!(a.starts_with("aish-"), "got: {a}");
        // A different checkout of the same-named repo gets a distinct key
        // (collision-safety preserved from the old FNV-of-abspath scheme).
        let c = fallback_repo_key(std::path::Path::new("/tmp/aish"));
        assert_ne!(a, c);
        assert!(c.starts_with("aish-"), "got: {c}");
    }

    #[test]
    fn should_sweep_keeps_dirty_or_ahead_reclaims_clean() {
        // Never reclaim a dirty or commits-ahead worktree — regardless of
        // orphan/age. This is the data-safety invariant.
        assert!(!should_sweep(true, false, true, 999, 7));
        assert!(!should_sweep(false, true, true, 999, 7));
        assert!(!should_sweep(true, true, false, 0, 7));
        // Clean + orphaned (registration gone) → reclaim immediately.
        assert!(should_sweep(false, false, true, 0, 7));
        // Clean + old enough → reclaim.
        assert!(should_sweep(false, false, false, 7, 7));
        assert!(should_sweep(false, false, false, 30, 7));
        // Clean but young and still registered → keep.
        assert!(!should_sweep(false, false, false, 6, 7));
        assert!(!should_sweep(false, false, false, 0, 7));
    }

    #[tokio::test]
    async fn stream_stderr_records_pulse_from_coordinator_lines() {
        // A realistic slice of a coordinator's piped stderr: a tool start line,
        // its success result line, a narration line, then a tool failure. The
        // pulse must end on the most recent event (the failure).
        let job = Arc::new(WorkerJob {
            id: "worker_1".into(),
            task: "t".into(),
            inner: Mutex::new(JobInner {
                status: "running".into(),
                pid: None,
            resumes: 0,
                result: None,
                error: None,
                displayed: false,
                branch: None,
                last_tool_outcome: None,
                last_turn_completion: None,
                transcript: VecDeque::new(),
                transcript_bytes: 0,
            backfill_spinner: None,
            started: SystemTime::now(),
            finished: None,
            }),
        });
        let lines = concat!(
            "\x1b[2m  \u{1f527} read /etc/hosts\x1b[0m\n",
            "\x1b[2m  \x1b[32m\u{2713}\x1b[0m \u{1f527} read /etc/hosts\x1b[0m\n",
            "\u{1f5e8} planning the next step\n",
            "\x1b[2m  \u{1f527} write x\x1b[0m\n",
            "\x1b[2m  \x1b[31m\u{2717}\x1b[0m \u{1f527} write x\x1b[0m\n",
        );
        let show = Arc::new(AtomicBool::new(false));
        let reader = lines.as_bytes();
        let attached = Arc::new(Mutex::new(None));
        let _tail = stream_stderr(reader, "worker_1", show, attached, Some(job.clone())).await;
        // Most recent event was the tool FAILURE -> red cross pulse.
        assert_eq!(job.latest_pulse().map(|(p, _)| p), Some(Pulse::ToolErr));
        // And it is fresh, so the aggregate badge is the red-cross variant.
        let jobs: WorkerJobs = Arc::new(Mutex::new(vec![job]));
        assert_eq!(
            pulse_badge(1, fresh_pulse(&jobs)),
            "\x1b[31m\u{2717}1\x1b[0m "
        );
    }

    #[test]
    fn classify_event_maps_tool_and_turn_lines() {
        // Coordinator non-TTY result lines carry a status glyph beside the wrench.
        assert_eq!(
            classify_event("\x1b[2m  ✓ 🔧 read /etc/hosts\x1b[0m"),
            Some(Pulse::ToolOk)
        );
        assert_eq!(
            classify_event("\x1b[2m  ✗ 🔧 write x\x1b[0m"),
            Some(Pulse::ToolErr)
        );
        // A bare start line (no ✓/✗) is the tool beginning, not an outcome.
        assert_eq!(classify_event("\x1b[2m  🔧 git status\x1b[0m"), None);
        // Turn narration carries the speech sentinel.
        assert_eq!(
            classify_event("🗨 planning the migration"),
            Some(Pulse::Turn)
        );
        // Noise lines carry nothing.
        assert_eq!(classify_event("coordinator run abc starting"), None);
        assert_eq!(classify_event(""), None);
        // A batch sentinel is not a pulse event.
        assert_eq!(classify_event("📦 fanned 3 sub-task(s) out"), None);
    }

    #[test]
    fn latest_pulse_picks_the_most_recent_event() {
        let job = Arc::new(WorkerJob {
            id: "worker_1".into(),
            task: "t".into(),
            inner: Mutex::new(JobInner {
                status: "running".into(),
                pid: None,
            resumes: 0,
                result: None,
                error: None,
                displayed: false,
                branch: None,
                last_tool_outcome: None,
                last_turn_completion: None,
                transcript: VecDeque::new(),
                transcript_bytes: 0,
            backfill_spinner: None,
            started: SystemTime::now(),
            finished: None,
            }),
        });
        // No events yet.
        assert!(job.latest_pulse().is_none());
        // A tool success, then (later) a turn completion: turn wins (most recent).
        job.record_tool_outcome(true);
        std::thread::sleep(std::time::Duration::from_millis(2));
        job.record_turn_completion();
        assert_eq!(job.latest_pulse().map(|(p, _)| p), Some(Pulse::Turn));
        // A still-later tool failure overtakes the turn.
        std::thread::sleep(std::time::Duration::from_millis(2));
        job.record_tool_outcome(false);
        assert_eq!(job.latest_pulse().map(|(p, _)| p), Some(Pulse::ToolErr));
    }

    #[test]
    fn fresh_pulse_aggregates_and_fades() {
        let jobs: WorkerJobs = Default::default();
        // Empty → nothing to pulse.
        assert_eq!(fresh_pulse(&jobs), None);
        let mk = |id: &str| {
            let j = Arc::new(WorkerJob {
                id: id.into(),
                task: "t".into(),
                inner: Mutex::new(JobInner {
                    status: "running".into(),
                pid: None,
            resumes: 0,
                    result: None,
                    error: None,
                    displayed: false,
                    branch: None,
                    last_tool_outcome: None,
                    last_turn_completion: None,
                    transcript: VecDeque::new(),
                    transcript_bytes: 0,
            backfill_spinner: None,
            started: SystemTime::now(),
            finished: None,
                }),
            });
            jobs.lock().unwrap().push(j.clone());
            j
        };
        let a = mk("worker_1");
        let b = mk("worker_2");
        a.record_tool_outcome(true);
        std::thread::sleep(std::time::Duration::from_millis(2));
        b.record_tool_outcome(false);
        // Most-recent across workers wins: b's failure.
        assert_eq!(fresh_pulse(&jobs), Some(Pulse::ToolErr));
        // A stale event (older than PULSE_FADE) fades out of the aggregate.
        {
            let mut i = b.inner.lock().unwrap();
            i.last_tool_outcome = Some((
                false,
                Instant::now() - PULSE_FADE - Duration::from_millis(50),
            ));
        }
        {
            let mut i = a.inner.lock().unwrap();
            i.last_tool_outcome = Some((
                true,
                Instant::now() - PULSE_FADE - Duration::from_millis(50),
            ));
        }
        assert_eq!(fresh_pulse(&jobs), None);
    }

    #[test]
    fn pulse_badge_colours_by_event_and_count() {
        // Nothing running → empty badge regardless of pulse.
        assert_eq!(pulse_badge(0, Some(Pulse::ToolOk)), "");
        // Idle (no recent event) → dim ⟳N.
        assert_eq!(pulse_badge(2, None), "\x1b[2m⟳2\x1b[0m ");
        // Tool success → green ✓N.
        assert_eq!(pulse_badge(1, Some(Pulse::ToolOk)), "\x1b[32m✓1\x1b[0m ");
        // Tool failure → red ✗N.
        assert_eq!(pulse_badge(1, Some(Pulse::ToolErr)), "\x1b[31m✗1\x1b[0m ");
        // Turn completion → bright magenta ⟳N.
        assert_eq!(pulse_badge(3, Some(Pulse::Turn)), "\x1b[1;35m⟳3\x1b[0m ");
    }

    #[test]
    fn describe_failure_names_sigkill_as_oom() {
        use std::os::unix::process::ExitStatusExt;
        // A status synthesised from signal 9 (SIGKILL).
        let killed = std::process::ExitStatus::from_raw(libc::SIGKILL);
        let msg = describe_failure(killed, "worker", "some stderr noise");
        assert!(msg.contains("killed by the OS"), "got: {msg}");
        assert!(msg.contains("AISH_WORKER_MEM_MB"), "got: {msg}");

        // A non-signal failure keeps the plain message and doesn't mention OOM.
        let exited = std::process::ExitStatus::from_raw(1 << 8); // exit code 1
        let msg = describe_failure(exited, "goal worker", "boom");
        assert!(msg.contains("exited unsuccessfully"), "got: {msg}");
        assert!(!msg.contains("killed by the OS"), "got: {msg}");
    }

    #[test]
    fn effective_worker_mem_mb_floors_low_values_for_v8() {
        // 0 == "no limit" is passed through untouched — the floor only lifts a
        // positive, too-small cap.
        assert_eq!(effective_worker_mem_mb(0), 0);
        // Anything below the runtime floor is raised so V8/Node (and Go/JVM) can
        // reserve their virtual cage — otherwise `neonctl` aborts at startup with
        // "Failed to reserve virtual memory for CodeRange".
        assert_eq!(effective_worker_mem_mb(1), MIN_WORKER_MEM_MB);
        assert_eq!(effective_worker_mem_mb(256), MIN_WORKER_MEM_MB);
        assert_eq!(effective_worker_mem_mb(1024), MIN_WORKER_MEM_MB);
        assert_eq!(
            effective_worker_mem_mb(MIN_WORKER_MEM_MB - 1),
            MIN_WORKER_MEM_MB
        );
        // At or above the floor the operator's value is honoured exactly.
        assert_eq!(effective_worker_mem_mb(MIN_WORKER_MEM_MB), MIN_WORKER_MEM_MB);
        assert_eq!(
            effective_worker_mem_mb(DEFAULT_WORKER_MEM_MB),
            DEFAULT_WORKER_MEM_MB
        );
        assert_eq!(effective_worker_mem_mb(8192), 8192);
        // Sanity: the default cap must itself be runnable (never below the floor),
        // so out-of-the-box coordinators can always launch Node-based tools.
        assert!(DEFAULT_WORKER_MEM_MB >= MIN_WORKER_MEM_MB);
    }

    #[test]
    fn worktree_add_backoff_is_exponential_10_to_160() {
        // Exact schedule required by the fix: 10, 20, 40, 80, 160 ms.
        let schedule: Vec<u64> = (1..=5)
            .map(|n| worktree_add_backoff(n).as_millis() as u64)
            .collect();
        assert_eq!(schedule, vec![10, 20, 40, 80, 160]);
        // A pathological retry index saturates instead of overflowing/panicking.
        let _ = worktree_add_backoff(1000);
    }

    #[test]
    fn is_worktree_lock_error_detects_lock_collisions() {
        assert!(is_worktree_lock_error(
            "fatal: another agent is live in this same working tree"
        ));
        assert!(is_worktree_lock_error(
            "fatal: 'wt' is already used by worktree at '/x'"
        ));
        assert!(is_worktree_lock_error("error: cannot lock ref"));
        assert!(is_worktree_lock_error("Unable to lock the index.lock"));
        // A genuinely fatal, non-lock error is NOT classified as a lock collision.
        assert!(!is_worktree_lock_error(
            "fatal: invalid reference: origin/nope"
        ));
        assert!(!is_worktree_lock_error(""));
    }

    #[test]
    fn retry_with_backoff_succeeds_on_a_later_attempt() {
        // Happy path: the operation fails the first two attempts (e.g. a lock
        // collision) then succeeds on the third. The driver returns true, took
        // exactly the backoff for the two failed tries (10 + 20 ms), and never
        // sleeps after the success.
        let mut attempts = 0u32;
        let mut sleeps: Vec<u64> = Vec::new();
        let ok = retry_with_backoff(
            WORKTREE_ADD_MAX_ATTEMPTS,
            |_n| {
                attempts += 1;
                attempts >= 3 // succeed on the 3rd attempt
            },
            |d| sleeps.push(d.as_millis() as u64),
        );
        assert!(ok, "must succeed once an attempt returns true");
        assert_eq!(attempts, 3, "stopped trying as soon as it succeeded");
        assert_eq!(
            sleeps,
            vec![10, 20],
            "backed off before each retry, not after success"
        );
    }

    #[test]
    fn retry_with_backoff_exhausts_then_falls_back() {
        // Exhaustion path: every attempt fails (a persistent collision). The
        // driver returns false after exactly MAX attempts, having slept the full
        // 4-gap schedule (10, 20, 40, 80) — no sleep after the final attempt. A
        // false return is what makes create_worktree fall back to the shared cwd.
        let mut attempts = 0u32;
        let mut sleeps: Vec<u64> = Vec::new();
        let ok = retry_with_backoff(
            WORKTREE_ADD_MAX_ATTEMPTS,
            |_n| {
                attempts += 1;
                false // never succeeds
            },
            |d| sleeps.push(d.as_millis() as u64),
        );
        assert!(
            !ok,
            "all attempts failed → false (caller falls back to shared cwd)"
        );
        assert_eq!(
            attempts, WORKTREE_ADD_MAX_ATTEMPTS,
            "tried exactly the max attempts"
        );
        assert_eq!(
            sleeps,
            vec![10, 20, 40, 80],
            "one backoff between each pair of attempts, none after the last"
        );
    }
}
