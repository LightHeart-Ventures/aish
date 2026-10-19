//! Agent-requested CHOICE popup (FR-431).
//!
//! An agent can ask the operator a structured question by emitting a fenced
//! block with the info string `choice`:
//!
//! ```text
//! ```choice
//! mode: one            # one | many | compare
//! question: Which deploy target should we use?
//! options:
//!   - id: staging
//!     label: Staging
//!     summary: Safe, ~5 min
//!   - id: prod
//!     label: Production
//!     summary: Live traffic, needs approval
//! ```
//! ```
//!
//! * `mode: one` — radio. Arrows/`j`/`k` move, Enter confirms.
//! * `mode: many` — checkboxes. Space toggles, Enter confirms.
//! * `mode: compare` — the options are laid out as rows of attributes
//!   (`summary` + any extra `key: value` pairs) with a single selection.
//!
//! Every popup also carries a free-text REPLY ROW as its last entry. It is a
//! row like any other — same caret and marker columns as the options, so it
//! lines up under them — markered `{ }` when idle and `{*}` when it holds the
//! answer in play, and up/down park on it the same way they park on an
//! option. Landing on it deselects the options in the single-select modes:
//! the operator's own words become the answer.
//!
//! Esc cancels and returns NO answer. Only the advertised keys act — every
//! other byte is dropped on the floor, matching the confirm prompt's
//! two-step discipline (`read_answer` in `repl.rs`).
//!
//! ## Layout contract
//!
//! The popup is painted in the BODY rows directly above the footer, so the
//! pinned escalation tray (which lives inside the footer region) stays visible
//! BELOW it — the popup pushes the tray's content up rather than over it, the
//! same invariant the recent tray fix established. The height is clamped by
//! [`popup_budget`], which asks the footer layout solver how many body rows
//! actually exist; a short terminal degrades to a scrolling window over the
//! option list, and a terminal with no room at all degrades to a plain inline
//! list (see [`ChoiceState::fits_inline_only`]).
//!
//! ## Answer routing
//!
//! A confirmed popup produces a JSON answer — `{"choice":"staging"}` for the
//! single-select modes, `{"choice":["a","b"]}` for `many` — which the REPL
//! feeds back into the RUNNING turn through the same `next_input` /
//! `continue 'resume` channel the loop-guard auto-recovery path uses. That is
//! the existing mechanism for returning operator input to a live agent; no new
//! transport was invented. A cancelled popup feeds nothing back and the raw
//! agent text is printed as usual.

use std::fmt::Write as _;

/// Selection semantics requested by the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChoiceMode {
    /// Radio: exactly one option, Enter confirms.
    #[default]
    One,
    /// Checkboxes: zero or more options, Space toggles, Enter confirms.
    Many,
    /// Attribute comparison with a single selection.
    Compare,
}

impl ChoiceMode {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "one" | "single" | "radio" => Some(Self::One),
            "many" | "multi" | "multiple" => Some(Self::Many),
            "compare" | "comparison" => Some(Self::Compare),
            _ => None,
        }
    }

    /// Whether Space toggles (and therefore whether the hint advertises it).
    pub fn multi(self) -> bool {
        matches!(self, Self::Many)
    }
}

/// One selectable option.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChoiceOption {
    /// The id handed back to the agent. Never shown as the primary label when a
    /// `label` exists, but it IS the answer.
    pub id: String,
    /// Human label. Falls back to `id` when the agent omitted it.
    pub label: String,
    /// One-line elaboration, rendered dimmed next to the label.
    pub summary: String,
    /// Extra `key: value` attributes, preserved in order. Used by `compare`.
    pub attrs: Vec<(String, String)>,
}

impl ChoiceOption {
    /// The text shown for this row (label, falling back to the id).
    pub fn display(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.id
        } else {
            &self.label
        }
    }
}

/// A parsed `choice` block.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChoiceSpec {
    pub mode: ChoiceMode,
    pub question: String,
    pub options: Vec<ChoiceOption>,
    /// Byte range of the whole fenced block in the source text, so the caller
    /// can strip it from what it prints.
    pub span: (usize, usize),
}

/// Why a block was rejected. A rejected block is NEVER fatal: the caller prints
/// the raw text instead, exactly as it would have before this feature existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceError {
    /// No `options:` entry carried a usable id.
    NoOptions,
    /// `question:` was missing or blank.
    NoQuestion,
}

/// Find the FIRST well-formed `choice` block in `text`.
///
/// Returns `None` both when there is no block at all and when the only block
/// present is malformed — the caller's fallback is identical in either case
/// (render the raw text), and a malformed block must never panic.
pub fn first_choice(text: &str) -> Option<ChoiceSpec> {
    all_choices(text).into_iter().next()
}

/// Every well-formed `choice` block in `text`, in source order. Malformed
/// blocks are skipped silently.
pub fn all_choices(text: &str) -> Vec<ChoiceSpec> {
    let mut out = Vec::new();
    for (body, span) in choice_blocks(text) {
        if let Ok(mut spec) = parse_choice_body(body) {
            spec.span = span;
            out.push(spec);
        }
    }
    out
}

/// Split out `(body, span)` for each ```choice fence. Tolerant of 3+ backticks,
/// leading indentation, and an unterminated final fence (which is skipped).
fn choice_blocks(text: &str) -> Vec<(&str, (usize, usize))> {
    let mut out = Vec::new();
    let mut idx = 0usize;
    let bytes_of = |line: &str| line.len();
    let mut lines: Vec<(usize, &str)> = Vec::new();
    for line in text.split_inclusive('\n') {
        lines.push((idx, line));
        idx += bytes_of(line);
    }
    let mut i = 0usize;
    while i < lines.len() {
        let (start, line) = lines[i];
        let trimmed = line.trim();
        let is_open = trimmed
            .strip_prefix("```")
            .map(|rest| rest.trim().eq_ignore_ascii_case("choice"))
            .unwrap_or(false);
        if !is_open {
            i += 1;
            continue;
        }
        let body_start = start + line.len();
        let mut j = i + 1;
        let mut body_end = None;
        let mut end = None;
        while j < lines.len() {
            let (off, l) = lines[j];
            if l.trim().starts_with("```") {
                body_end = Some(off);
                end = Some(off + l.len());
                break;
            }
            j += 1;
        }
        match (body_end, end) {
            (Some(be), Some(e)) => {
                out.push((&text[body_start..be], (start, e)));
                i = j + 1;
            }
            // Unterminated fence: ignore it rather than swallowing the tail of
            // the message.
            _ => break,
        }
    }
    out
}

/// Parse the YAML-ish body of a `choice` block.
///
/// Deliberately a tolerant SUBSET, not a YAML implementation: `key: value`
/// scalars at column 0, an `options:` list of `- id: …` items with indented
/// continuation keys, `#` comments, blank lines, and optional surrounding
/// quotes. Anything it cannot understand is skipped, and a body with no usable
/// option or no question is an error the caller degrades on.
pub fn parse_choice_body(body: &str) -> Result<ChoiceSpec, ChoiceError> {
    let mut spec = ChoiceSpec::default();
    let mut in_options = false;
    for raw in body.lines() {
        let line = strip_comment(raw);
        if line.trim().is_empty() {
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        let trimmed = line.trim();

        if let Some(rest) = trimmed.strip_prefix("- ") {
            // A new option item. `- id: x` or a bare `- x`.
            if !in_options {
                continue;
            }
            let mut opt = ChoiceOption::default();
            apply_option_field(&mut opt, rest);
            spec.options.push(opt);
            continue;
        }

        let Some((key, value)) = split_kv(trimmed) else {
            continue;
        };
        let key_l = key.to_ascii_lowercase();

        if !indented {
            in_options = false;
            match key_l.as_str() {
                "mode" => {
                    if let Some(m) = ChoiceMode::parse(value) {
                        spec.mode = m;
                    }
                }
                "question" | "prompt" | "title" => spec.question = unquote(value).to_string(),
                "options" | "choices" => in_options = true,
                _ => {}
            }
            continue;
        }

        // Indented `key: value` under the current option.
        if in_options && let Some(opt) = spec.options.last_mut() {
            apply_option_field(opt, trimmed);
        }
    }

    spec.options.retain(|o| !o.id.trim().is_empty());
    if spec.options.is_empty() {
        return Err(ChoiceError::NoOptions);
    }
    if spec.question.trim().is_empty() {
        return Err(ChoiceError::NoQuestion);
    }
    Ok(spec)
}

fn apply_option_field(opt: &mut ChoiceOption, text: &str) {
    match split_kv(text) {
        Some((key, value)) => {
            let v = unquote(value).to_string();
            match key.to_ascii_lowercase().as_str() {
                "id" | "value" => opt.id = v,
                "label" | "name" | "title" => opt.label = v,
                "summary" | "detail" | "description" => opt.summary = v,
                other => opt.attrs.push((other.to_string(), v)),
            }
        }
        // A bare `- staging` is both the id and the label.
        None => {
            let v = unquote(text.trim()).to_string();
            if !v.is_empty() {
                opt.id = v;
            }
        }
    }
}

fn split_kv(s: &str) -> Option<(&str, &str)> {
    let (k, v) = s.split_once(':')?;
    let k = k.trim();
    if k.is_empty() || k.contains(' ') && k.len() > 40 {
        return None;
    }
    Some((k, v.trim()))
}

/// Drop a trailing ` # comment`, but never inside quotes.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    for (i, &b) in bytes.iter().enumerate() {
        match (quote, b) {
            (None, b'"') | (None, b'\'') => quote = Some(b),
            (Some(q), c) if c == q => quote = None,
            // Only a `#` that follows whitespace (or starts the line) is a
            // comment — `id: a#b` keeps its hash.
            (None, b'#') if i == 0 || bytes[i - 1].is_ascii_whitespace() => {
                return &line[..i];
            }
            _ => {}
        }
    }
    line
}

fn unquote(s: &str) -> &str {
    let t = s.trim();
    for q in ['"', '\''] {
        if t.len() >= 2 && t.starts_with(q) && t.ends_with(q) {
            return &t[1..t.len() - 1];
        }
    }
    t
}

// ---------------------------------------------------------------------------
// Selection state
// ---------------------------------------------------------------------------

/// What a key did to the popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing advertised happened — redraw is unnecessary.
    Ignored,
    /// The cursor or the selection moved; repaint.
    Redraw,
    /// Enter with a usable selection: the JSON answer to feed back.
    Confirmed(String),
    /// Esc / Ctrl-C: no answer at all.
    Cancelled,
}

/// The advertised keys. Everything else is `Key::Other` and does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Toggle,
    Confirm,
    Cancel,
    /// A printable character. Whether it MOVES the cursor or lands in the reply
    /// line is context, not decoding: see [`ChoiceState::apply`].
    Char(char),
    /// Backspace / DEL — edits the reply line.
    Backspace,
    Other,
}

/// Decode a keystroke from the bytes a raw tty hands over.
///
/// `bytes` is one keypress: a single byte, or an ESC-introduced sequence.
/// Printable ASCII decodes to [`Key::Char`] — including `j`/`k`/space, which
/// the popup still treats as movement/toggle while the reply line is empty.
/// Keeping that decision in [`ChoiceState::apply`] rather than here is what
/// lets the operator type a reply that STARTS with any letter once they have
/// begun typing, instead of losing three characters to the key map forever.
pub fn decode_key(bytes: &[u8]) -> Key {
    match bytes {
        [0x1b] => Key::Cancel,
        // CSI / SS3 arrows. Anything else ESC-introduced is inert.
        [0x1b, b'[', b'A', ..] | [0x1b, b'O', b'A', ..] => Key::Up,
        [0x1b, b'[', b'B', ..] | [0x1b, b'O', b'B', ..] => Key::Down,
        [b'\r'] | [b'\n'] => Key::Confirm,
        // Ctrl-C / Ctrl-D cancel, same as the confirm prompt.
        [0x03] | [0x04] => Key::Cancel,
        // DEL / Ctrl-H edit the reply line; Tab toggles in `many` mode so the
        // space bar is free to be a literal space once typing starts.
        [0x7f] | [0x08] => Key::Backspace,
        [b'\t'] => Key::Toggle,
        [c] if (0x20..0x7f).contains(c) => Key::Char(*c as char),
        _ => Key::Other,
    }
}

/// Cursor + selection for an open popup.
#[derive(Debug, Clone)]
pub struct ChoiceState {
    spec: ChoiceSpec,
    cursor: usize,
    checked: Vec<bool>,
    /// First visible option row — the scroll window for short terminals.
    top: usize,
    /// How many option rows the terminal can show at once (>=1).
    window: usize,
    /// Free-text the operator typed on the always-present reply line.
    reply: String,
}

impl ChoiceState {
    pub fn new(spec: ChoiceSpec) -> Self {
        let n = spec.options.len();
        Self {
            spec,
            cursor: 0,
            checked: vec![false; n],
            top: 0,
            window: n.max(1),
            reply: String::new(),
        }
    }

    /// What the operator has typed on the reply line so far.
    #[allow(dead_code)] // part of the popup's public surface; used by tests today
    pub fn reply(&self) -> &str {
        &self.reply
    }

    #[allow(dead_code)] // part of the popup's public surface; used by tests today
    pub fn spec(&self) -> &ChoiceSpec {
        &self.spec
    }

    #[allow(dead_code)] // part of the popup's public surface; used by tests today
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Ids currently checked (`many`) or under the cursor (single-select).
    pub fn selected_ids(&self) -> Vec<String> {
        if self.spec.mode.multi() {
            self.spec
                .options
                .iter()
                .zip(&self.checked)
                .filter(|(_, on)| **on)
                .map(|(o, _)| o.id.clone())
                .collect()
        } else {
            self.spec
                .options
                .get(self.cursor)
                .map(|o| vec![o.id.clone()])
                .unwrap_or_default()
        }
    }

    /// Clamp the visible window to `rows` option rows. A `rows` of 0 means the
    /// popup cannot be drawn at all — see [`Self::fits_inline_only`].
    pub fn set_window(&mut self, rows: usize) {
        self.window = rows.max(1);
        self.scroll_into_view();
    }

    /// True when the terminal has no room for a framed popup and the caller
    /// should fall back to a plain inline list.
    #[allow(dead_code)] // callers outside the popup loop use this to pick inline mode
    pub fn fits_inline_only(&self, body_rows: u16) -> bool {
        popup_budget(body_rows) == 0
    }

    /// True when the cursor is parked on the free-text reply row — the last
    /// seat in the ring, past the final option.
    pub fn on_reply_row(&self) -> bool {
        self.cursor >= self.spec.options.len()
    }

    fn scroll_into_view(&mut self) {
        // The reply row lives OUTSIDE the scrolling option window (it is
        // chrome, always painted last), so parking on it must not scroll.
        if self.on_reply_row() {
            return;
        }
        if self.cursor < self.top {
            self.top = self.cursor;
        }
        let last = self.top + self.window.saturating_sub(1);
        if self.cursor > last {
            self.top = self.cursor + 1 - self.window;
        }
        let max_top = self.spec.options.len().saturating_sub(self.window);
        self.top = self.top.min(max_top);
    }

    /// Apply one decoded key.
    pub fn apply(&mut self, key: Key) -> Outcome {
        let n = self.spec.options.len();
        // The reply row is the last seat in the ring: `n` options plus the
        // free-text row, so up/down land ON the operator's own answer the
        // same way they land on an option instead of skipping past it.
        let ring = n + 1;
        match key {
            Key::Up => {
                self.cursor = if self.cursor == 0 {
                    ring - 1
                } else {
                    self.cursor - 1
                };
                self.scroll_into_view();
                Outcome::Redraw
            }
            Key::Down => {
                self.cursor = (self.cursor + 1) % ring;
                self.scroll_into_view();
                Outcome::Redraw
            }
            Key::Toggle if self.spec.mode.multi() => {
                if let Some(slot) = self.checked.get_mut(self.cursor) {
                    *slot = !*slot;
                    return Outcome::Redraw;
                }
                Outcome::Ignored
            }
            // Printable keys. While the reply line is still EMPTY *and* the
            // cursor is parked on an option, the legacy selection keys win
            // (j/k move, space toggles); the moment the operator has typed
            // anything — or has moved onto the reply row, where literal text
            // is the whole point — every printable is text.
            Key::Char(c) => {
                if self.reply.is_empty() && !self.on_reply_row() {
                    match c {
                        'j' | 'J' if n > 0 => return self.apply(Key::Down),
                        'k' | 'K' if n > 0 => return self.apply(Key::Up),
                        ' ' if self.spec.mode.multi() => return self.apply(Key::Toggle),
                        _ => {}
                    }
                }
                self.reply.push(c);
                Outcome::Redraw
            }
            Key::Backspace => {
                if self.reply.pop().is_some() {
                    Outcome::Redraw
                } else {
                    Outcome::Ignored
                }
            }
            Key::Confirm => {
                let ids = self.selected_ids();
                // Nothing ticked AND nothing typed is not an answer — keep
                // waiting, the same way a bare Enter at the confirm prompt
                // does nothing. A typed reply alone IS an answer: that is the
                // point of the reply line.
                if ids.is_empty() && self.reply.trim().is_empty() {
                    return Outcome::Ignored;
                }
                Outcome::Confirmed(answer_json_with_reply(
                    self.spec.mode,
                    &ids,
                    self.reply.trim(),
                ))
            }
            Key::Cancel => Outcome::Cancelled,
            _ => Outcome::Ignored,
        }
    }

    /// The popup as styled rows, newest layout first. ASCII only — no emoji,
    /// and no reliance on colour to convey the selection.
    pub fn render(&self, cols: u16, color: bool) -> Vec<String> {
        let width = (cols as usize).clamp(20, 200);
        let dim = |s: &str| {
            if color {
                format!("\x1b[2m{s}\x1b[0m")
            } else {
                s.to_string()
            }
        };
        let hi = |s: &str| {
            if color {
                format!("\x1b[1;36m{s}\x1b[0m")
            } else {
                s.to_string()
            }
        };
        let mut rows = Vec::new();
        rows.push(hi(&clip(&format!("? {}", self.spec.question), width)));
        let end = (self.top + self.window).min(self.spec.options.len());
        if self.top > 0 {
            rows.push(dim(&format!("  ... {} above", self.top)));
        }
        for (i, opt) in self.spec.options[self.top..end].iter().enumerate() {
            let idx = self.top + i;
            let here = idx == self.cursor;
            let marker = match (self.spec.mode.multi(), self.checked[idx], here) {
                (true, true, _) => "[x]",
                (true, false, _) => "[ ]",
                (false, _, true) => "(*)",
                (false, _, false) => "( )",
            };
            let caret = if here { ">" } else { " " };
            let mut line = String::new();
            let _ = write!(line, "{caret} {marker} {}", opt.display());
            if !opt.summary.trim().is_empty() {
                let _ = write!(line, "  -- {}", opt.summary);
            }
            if matches!(self.spec.mode, ChoiceMode::Compare) {
                for (k, v) in &opt.attrs {
                    let _ = write!(line, "  | {k}: {v}");
                }
            }
            let clipped = clip(&line, width);
            rows.push(if here { hi(&clipped) } else { clipped });
        }
        let remaining = self.spec.options.len().saturating_sub(end);
        if remaining > 0 {
            rows.push(dim(&format!("  ... {remaining} more")));
        }
        // The reply row, ALWAYS present and always the last seat — rendered
        // in the SAME `caret marker text` shape as an option so it lines up
        // with them instead of hanging off on its own `>` line. Its marker is
        // `{ }` / `{*}`: brace-shaped to read as free text rather than a
        // fixed choice, selectable exactly like `( )` / `[ ]`.
        let here = self.on_reply_row();
        let marker = if here { "{*}" } else { "{ }" };
        let caret = if here { ">" } else { " " };
        let mut line = String::new();
        let _ = write!(line, "{caret} {marker} ");
        if self.reply.is_empty() && !here {
            // Untouched and unfocused: name itself, dimmed, so the row reads
            // as an offer rather than an empty mystery.
            line.push_str(REPLY_PLACEHOLDER);
            rows.push(dim(&clip(&line, width)));
        } else {
            line.push_str(&self.reply);
            line.push('_');
            let clipped = clip(&line, width);
            rows.push(if here { hi(&clipped) } else { clipped });
        }
        rows.push(dim(&clip(self.hint(), width)));
        rows
    }

    /// The advertised keys, verbatim. If a key is not in this string it does
    /// nothing — that is the contract the confirm prompt set.
    pub fn hint(&self) -> &'static str {
        if self.spec.mode.multi() {
            "up/down move - space/tab toggles - type to reply - enter confirms - esc cancels"
        } else {
            "up/down move - type to reply - enter confirms - esc cancels"
        }
    }

    /// Rows this popup wants, given its current window.
    #[allow(dead_code)] // reserved-row accounting for callers that pre-measure
    pub fn height(&self) -> usize {
        let visible = self.window.min(self.spec.options.len());
        // question + options + hint + reply, plus the scroll affordances when
        // clipped.
        CHROME_ROWS
            + visible
            + usize::from(self.top > 0)
            + usize::from(self.top + visible < self.spec.options.len())
    }
}

/// Non-option rows every popup paints: the question, the hint, and the
/// always-present reply row.
pub const CHROME_ROWS: usize = 3;

/// What the empty, unfocused reply row says about itself. Short enough to sit
/// inside the option column without wrapping on a narrow terminal.
const REPLY_PLACEHOLDER: &str = "type to reply";

/// JSON answer plus the operator's typed reply, when they typed one. The
/// `choice` key keeps its exact old shape so existing consumers are unaffected;
/// `reply` is additive and omitted when empty.
pub fn answer_json_with_reply(mode: ChoiceMode, ids: &[String], reply: &str) -> String {
    let base = answer_json(mode, ids);
    if reply.is_empty() {
        return base;
    }
    let body = base.strip_suffix('}').unwrap_or(&base);
    format!("{body},\"reply\":\"{}\"}}", escape(reply))
}

/// JSON answer for a confirmed popup: an object with a single `choice` key,
/// scalar for single-select and an array for `many`.
pub fn answer_json(mode: ChoiceMode, ids: &[String]) -> String {
    if mode.multi() {
        let body = ids
            .iter()
            .map(|id| format!("\"{}\"", escape(id)))
            .collect::<Vec<_>>()
            .join(",");
        format!("{{\"choice\":[{body}]}}")
    } else {
        format!(
            "{{\"choice\":\"{}\"}}",
            escape(ids.first().map_or("", |s| s))
        )
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn clip(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let keep = width.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('~');
    out
}

/// How many rows a popup may occupy in a window whose BODY is `body_rows` tall.
///
/// The footer — and with it the pinned escalation tray — is never encroached
/// on: `body_rows` is what the footer layout solver already left over, and the
/// popup takes at most half of it (minimum 4: question + one option + hint +
/// a scroll affordance). `0` means "no framed popup fits, print inline".
pub fn popup_budget(body_rows: u16) -> usize {
    let body = body_rows as usize;
    if body < 4 {
        return 0;
    }
    (body / 2).max(4).min(body.saturating_sub(1))
}

/// Option rows the popup can show for a `body_rows`-tall body: the budget less
/// the chrome (question row, hint row, and the always-present reply line).
pub fn option_window(body_rows: u16) -> usize {
    popup_budget(body_rows).saturating_sub(CHROME_ROWS)
}

/// Body rows available above the footer for the CURRENT terminal height, via
/// the footer layout solver. `want_banner_rows` is the pinned-escalation height
/// the tray is asking for, so a popup opened while the tray is visible shares
/// the window with it instead of overwriting it.
pub fn body_rows_for(rows: u16, want_banner_rows: u16) -> u16 {
    let plan = crate::terminal::FooterLayout::solve(rows, want_banner_rows);
    if plan.height == 0 {
        // No footer at all (tiny window): the whole screen is body.
        return rows;
    }
    plan.body_bottom
}

// ---------------------------------------------------------------------------
// Interactive runner
// ---------------------------------------------------------------------------

/// Terminal size, or `None` when stdin/stdout is not a tty we can measure.
#[cfg(unix)]
fn term_size() -> Option<(u16, u16)> {
    // SAFETY: a single TIOCGWINSZ ioctl on fd 1 into a zeroed, correctly-sized
    // struct; the return code is checked before the fields are read.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) != 0 || ws.ws_row == 0 {
            return None;
        }
        Some((ws.ws_row, ws.ws_col.max(20)))
    }
}

/// Read ONE keypress from the raw tty, decoding ESC-introduced sequences.
///
/// Mirrors `repl::read_key_tty`'s discipline — ICANON/ECHO/ISIG cleared so
/// Ctrl-C arrives as data (aish ignores SIGINT process-wide), original attrs
/// restored on every exit path — but KEEPS the ESC tail instead of draining it,
/// because here the arrow keys are advertised input rather than noise.
#[cfg(unix)]
fn read_choice_key() -> Option<Key> {
    // SAFETY: termios get/set plus short reads on fd 0, every rc checked, and
    // the cooked attrs restored before each return.
    unsafe {
        let mut cooked: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut cooked) != 0 {
            return None;
        }
        let mut raw = cooked;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        raw.c_iflag &= !(libc::IXON | libc::IXOFF);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
            return None;
        }
        let mut buf = [0u8; 8];
        let n = libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, 1);
        let mut len = if n == 1 { 1usize } else { 0 };
        if len == 1 && buf[0] == 0x1b {
            // Peek the tail with a 100ms timeout: present => arrow key,
            // absent => a bare Esc, which cancels.
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 1;
            libc::tcsetattr(0, libc::TCSANOW, &raw);
            let more = libc::read(0, buf.as_mut_ptr().add(1) as *mut libc::c_void, 7);
            if more > 0 {
                len += more as usize;
            }
        }
        libc::tcsetattr(0, libc::TCSANOW, &cooked);
        if len == 0 {
            return None;
        }
        Some(decode_key(&buf[..len]))
    }
}

/// Open the popup on the controlling tty and block until the operator answers.
///
/// Returns the JSON answer on Enter, `None` on Esc / Ctrl-C / a tty we cannot
/// drive. `want_banner_rows` is the pinned-escalation height currently asked
/// for by the tray, so the popup is sized against the body the footer layout
/// solver left over — the tray keeps its rows and stays visible BELOW the
/// popup, which is the whole point of painting here rather than over it.
#[cfg(unix)]
pub fn prompt_choice(spec: ChoiceSpec, want_banner_rows: u16) -> Option<String> {
    use std::io::Write as _;
    // SAFETY: isatty is a pure query on fd 0.
    if unsafe { libc::isatty(0) } != 1 {
        return None;
    }
    let (rows, cols) = term_size()?;
    let mut state = ChoiceState::new(spec);

    // Preferred path: hand the popup to the footer's hint tray. The layout
    // solver seats that tray BELOW the pinned escalation banners and flush
    // ABOVE the horizontal rule, so the menu lands where the operator expects
    // it — bottom-anchored, under the escalations — and growing it pushes the
    // banner stack UP rather than painting over the footer. Falls through to
    // the inline paint below when there is no footer region to hang it on.
    if let Some(cap) = crate::terminal::tray_capacity_full() {
        state.set_window(cap.saturating_sub(CHROME_ROWS).max(1));
        if crate::terminal::set_hint_tray(state.render(cols, true)) {
            let answer = loop {
                let Some(key) = read_choice_key() else {
                    break None;
                };
                match state.apply(key) {
                    Outcome::Ignored => continue,
                    Outcome::Redraw => {
                        crate::terminal::set_hint_tray(state.render(cols, true));
                    }
                    Outcome::Confirmed(answer) => break Some(answer),
                    Outcome::Cancelled => break None,
                }
            };
            crate::terminal::clear_hint_tray();
            return answer;
        }
    }

    let body = body_rows_for(rows, want_banner_rows);
    let window = option_window(body);
    if window == 0 {
        // Degraded window: print the list once, inline, and take no keys —
        // a popup that cannot be redrawn safely is worse than plain text.
        for row in state.render(cols, false) {
            println!("{row}");
        }
        return None;
    }
    state.set_window(window);

    let mut painted = 0usize;
    let mut out = std::io::stdout();
    loop {
        if painted > 0 {
            // Rewind over the previous paint and clear downward. The footer —
            // and the tray inside it — is below the cursor and untouched.
            let _ = write!(out, "\x1b[{painted}A\x1b[J");
        }
        let rendered = state.render(cols, true);
        for row in &rendered {
            let _ = writeln!(out, "{row}");
        }
        let _ = out.flush();
        painted = rendered.len();

        let key = read_choice_key()?;
        match state.apply(key) {
            Outcome::Ignored | Outcome::Redraw => continue,
            Outcome::Confirmed(answer) => return Some(answer),
            Outcome::Cancelled => {
                let _ = writeln!(out, "\x1b[2mcancelled\x1b[0m");
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verbatim from the FR-431 spec — a raw literal so the option
    // indentation that the parser keys off survives into the fixture.
    const ONE: &str = r#"intro text
```choice
mode: one            # one | many | compare
question: Which deploy target should we use?
options:
  - id: staging
    label: Staging
    summary: Safe, ~5 min
  - id: prod
    label: Production
    summary: Live traffic, needs approval
```
trailing text
"#;

    #[test]
    fn parses_the_documented_one_block() {
        let spec = first_choice(ONE).expect("block must parse");
        assert_eq!(spec.mode, ChoiceMode::One);
        assert_eq!(spec.question, "Which deploy target should we use?");
        assert_eq!(spec.options.len(), 2);
        assert_eq!(spec.options[0].id, "staging");
        assert_eq!(spec.options[0].label, "Staging");
        assert_eq!(spec.options[0].summary, "Safe, ~5 min");
        assert_eq!(spec.options[1].id, "prod");
        // The span covers the fence so the caller can strip it.
        let block = &ONE[spec.span.0..spec.span.1];
        assert!(
            block.starts_with("```choice"),
            "span must start at the fence"
        );
        assert!(
            block.trim_end().ends_with("```"),
            "span must end at the fence"
        );
    }

    #[test]
    fn parses_many_and_compare_modes() {
        let many =
            "```choice\nmode: many\nquestion: Pick some\noptions:\n  - id: a\n  - id: b\n```\n";
        let spec = first_choice(many).expect("many must parse");
        assert_eq!(spec.mode, ChoiceMode::Many);
        assert!(spec.mode.multi());

        let compare = "```choice\nmode: compare\nquestion: Compare\noptions:\n  - id: a\n    label: A\n    cost: low\n  - id: b\n    label: B\n    cost: high\n```\n";
        let spec = first_choice(compare).expect("compare must parse");
        assert_eq!(spec.mode, ChoiceMode::Compare);
        assert_eq!(
            spec.options[0].attrs,
            vec![("cost".to_string(), "low".to_string())]
        );
        assert!(!spec.mode.multi(), "compare is single-select");
    }

    #[test]
    fn mode_defaults_to_one_when_absent_or_unknown() {
        let s = "```choice\nquestion: q\noptions:\n  - id: a\n```\n";
        assert_eq!(first_choice(s).unwrap().mode, ChoiceMode::One);
        let s = "```choice\nmode: sideways\nquestion: q\noptions:\n  - id: a\n```\n";
        assert_eq!(first_choice(s).unwrap().mode, ChoiceMode::One);
    }

    #[test]
    fn malformed_blocks_degrade_instead_of_panicking() {
        for bad in [
            "```choice\n```\n",                              // empty
            "```choice\nquestion: q\n```\n",                 // no options
            "```choice\noptions:\n  - id: a\n```\n",         // no question
            "```choice\nmode: one\nquestion: q\noptions:\n", // unterminated
            "```choice\n: : :\n- - -\n\0\n```\n",            // garbage
            "no fence here at all",
            "",
        ] {
            assert!(first_choice(bad).is_none(), "{bad:?} must not yield a spec");
        }
        // And the error is classified, not a panic.
        assert_eq!(
            parse_choice_body("question: q\n"),
            Err(ChoiceError::NoOptions)
        );
        assert_eq!(
            parse_choice_body("options:\n  - id: a\n"),
            Err(ChoiceError::NoQuestion)
        );
    }

    #[test]
    fn tolerates_comments_quotes_and_bare_items() {
        let s = "```choice\n# leading comment\nmode: many   # trailing\nquestion: \"Pick #1?\"\noptions:\n  - staging\n  - id: prod\n```\n";
        let spec = first_choice(s).expect("tolerant parse");
        assert_eq!(spec.question, "Pick #1?");
        assert_eq!(spec.options[0].id, "staging");
        assert_eq!(
            spec.options[0].display(),
            "staging",
            "bare item is its own label"
        );
        assert_eq!(spec.options[1].id, "prod");
    }

    #[test]
    fn only_advertised_keys_decode() {
        // Control keys decode to intents...
        assert_eq!(decode_key(b"\x1b[A"), Key::Up);
        assert_eq!(decode_key(b"\x1b[B"), Key::Down);
        assert_eq!(decode_key(b"\x1bOA"), Key::Up);
        assert_eq!(decode_key(b"\t"), Key::Toggle);
        assert_eq!(decode_key(b"\r"), Key::Confirm);
        assert_eq!(decode_key(b"\x1b"), Key::Cancel);
        assert_eq!(decode_key(&[0x03]), Key::Cancel);
        assert_eq!(decode_key(&[0x7f]), Key::Backspace);
        // ...and every printable is text at this layer. j/k/space only MEAN
        // movement in `apply`, and only while the reply line is empty.
        assert_eq!(decode_key(b"k"), Key::Char('k'));
        assert_eq!(decode_key(b"j"), Key::Char('j'));
        assert_eq!(decode_key(b" "), Key::Char(' '));
        assert_eq!(decode_key(b"Z"), Key::Char('Z'));
        for junk in [&b"\x1b[C"[..], b"\x1b[D", &[0x01], &[0x1b, b'[', b'Z']] {
            assert_eq!(decode_key(junk), Key::Other, "{junk:?} must be inert");
        }
    }

    #[test]
    fn reply_line_is_always_rendered_last_and_answers_alone() {
        let mut st = state("one", 2);
        // Always present, even untouched: an option-shaped `{ }` row that sits
        // in the same column as the real options, directly above the hint.
        let rows = st.render(80, false);
        assert!(
            rows.last()
                .unwrap()
                .contains("type to reply - enter confirms")
        );
        assert_eq!(
            reply_row(&rows).trim_end(),
            "  { } type to reply",
            "got {rows:?}"
        );

        // Typing lands in the reply row once the operator starts a word.
        for c in "ok".chars() {
            assert_eq!(st.apply(Key::Char(c)), Outcome::Redraw);
        }
        assert_eq!(st.reply(), "ok");
        assert_eq!(reply_row(&st.render(80, false)).trim_end(), "  { } ok_");

        // Backspace edits it; an empty line swallows nothing.
        assert_eq!(st.apply(Key::Backspace), Outcome::Redraw);
        assert_eq!(st.reply(), "o");
        st.apply(Key::Backspace);
        assert_eq!(st.apply(Key::Backspace), Outcome::Ignored);

        // A typed reply rides along with the choice...
        st.apply(Key::Char('h'));
        st.apply(Key::Char('i'));
        assert_eq!(
            st.apply(Key::Confirm),
            Outcome::Confirmed("{\"choice\":\"o0\",\"reply\":\"hi\"}".to_string())
        );

        // ...and in `many` mode a reply alone is a valid answer, while an
        // empty popup with nothing typed still ignores Enter.
        let mut st = state("many", 2);
        assert_eq!(st.apply(Key::Confirm), Outcome::Ignored);
        st.apply(Key::Char('n'));
        st.apply(Key::Char('o'));
        assert_eq!(
            st.apply(Key::Confirm),
            Outcome::Confirmed("{\"choice\":[],\"reply\":\"no\"}".to_string())
        );
    }

    #[test]
    fn selection_keys_win_only_while_the_reply_is_empty() {
        let mut st = state("many", 3);
        // Empty reply: j/k move, space toggles — the old muscle memory.
        st.apply(Key::Char('j'));
        assert_eq!(st.cursor(), 1);
        st.apply(Key::Char('k'));
        assert_eq!(st.cursor(), 0);
        st.apply(Key::Char(' '));
        assert_eq!(st.selected_ids(), vec!["o0".to_string()]);
        assert!(st.reply().is_empty(), "movement keys are not text");

        // Once typing starts, every printable is literal — including j/k and
        // spaces, so the operator can write a sentence.
        st.apply(Key::Char('n'));
        for c in "o j k".chars() {
            st.apply(Key::Char(c));
        }
        assert_eq!(st.reply(), "no j k");
        assert_eq!(st.cursor(), 0, "cursor must not have moved");
        // Arrows and Tab still work regardless of what has been typed.
        st.apply(Key::Down);
        assert_eq!(st.cursor(), 1);
        st.apply(Key::Toggle);
        assert_eq!(st.selected_ids(), vec!["o0".to_string(), "o1".to_string()]);
    }

    /// The reply row is the second-to-last row: options, reply, hint.
    fn reply_row(rows: &[String]) -> &str {
        &rows[rows.len() - 2]
    }

    fn state(mode: &str, n: usize) -> ChoiceState {
        let mut body = format!("mode: {mode}\nquestion: q\noptions:\n");
        for i in 0..n {
            body.push_str(&format!("  - id: o{i}\n    label: O{i}\n"));
        }
        ChoiceState::new(parse_choice_body(&body).expect("fixture must parse"))
    }

    #[test]
    fn one_mode_moves_and_confirms_the_cursor() {
        let mut st = state("one", 3);
        assert_eq!(st.apply(Key::Down), Outcome::Redraw);
        assert_eq!(st.cursor(), 1);
        // Space does nothing in single-select.
        assert_eq!(st.apply(Key::Toggle), Outcome::Ignored);
        assert_eq!(st.apply(Key::Other), Outcome::Ignored);
        assert_eq!(
            st.apply(Key::Confirm),
            Outcome::Confirmed("{\"choice\":\"o1\"}".to_string())
        );
        // Wrap-around in both directions, THROUGH the reply row — it is the
        // last seat in the ring, so Up from the first option lands on it.
        let mut st = state("one", 3);
        assert_eq!(st.apply(Key::Up), Outcome::Redraw);
        assert!(st.on_reply_row(), "up from the top parks on the reply row");
        assert_eq!(st.apply(Key::Down), Outcome::Redraw);
        assert_eq!(st.cursor(), 0);
        assert_eq!(st.apply(Key::Up), Outcome::Redraw);
        assert_eq!(st.apply(Key::Up), Outcome::Redraw);
        assert_eq!(st.cursor(), 2, "and the option above it is the last one");
    }

    #[test]
    fn the_reply_row_is_selectable_like_any_option() {
        let mut st = state("one", 2);
        // Park on it: the marker fills in and the caret moves down with it.
        st.apply(Key::Up);
        assert!(st.on_reply_row());
        let rows = st.render(80, false);
        assert_eq!(reply_row(&rows).trim_end(), "> {*} _", "got {rows:?}");
        assert!(rows[1].starts_with("  ( ) O0"), "no option is marked now");
        assert!(rows[2].starts_with("  ( ) O1"), "got {:?}", rows[2]);

        // Typing shows up in place, and j/k/space stay literal text here
        // because the reply row is where free text belongs.
        for c in "no j k".chars() {
            st.apply(Key::Char(c));
        }
        assert_eq!(st.reply(), "no j k");
        assert!(st.on_reply_row(), "typing must not move the cursor");
        assert_eq!(reply_row(&st.render(80, false)).trim_end(), "> {*} no j k_");

        // Space/tab is not a toggle on this row, and Enter answers with the
        // text alone — no option id, because none was picked.
        assert_eq!(st.apply(Key::Toggle), Outcome::Ignored);
        assert_eq!(
            st.apply(Key::Confirm),
            Outcome::Confirmed("{\"choice\":\"\",\"reply\":\"no j k\"}".to_string())
        );
    }

    #[test]
    fn many_mode_toggles_and_returns_every_checked_id() {
        let mut st = state("many", 3);
        // Enter with nothing ticked is not an answer.
        assert_eq!(st.apply(Key::Confirm), Outcome::Ignored);
        assert_eq!(st.apply(Key::Toggle), Outcome::Redraw);
        st.apply(Key::Down);
        st.apply(Key::Down);
        st.apply(Key::Toggle);
        assert_eq!(st.selected_ids(), vec!["o0".to_string(), "o2".to_string()]);
        assert_eq!(
            st.apply(Key::Confirm),
            Outcome::Confirmed("{\"choice\":[\"o0\",\"o2\"]}".to_string())
        );
        // Toggling off again clears it.
        st.apply(Key::Toggle);
        assert_eq!(st.selected_ids(), vec!["o0".to_string()]);
    }

    #[test]
    fn esc_cancels_with_no_answer() {
        let mut st = state("many", 2);
        st.apply(Key::Toggle);
        assert_eq!(st.apply(Key::Cancel), Outcome::Cancelled);
    }

    #[test]
    fn render_marks_the_cursor_without_emoji_or_colour() {
        let mut st = state("many", 2);
        st.apply(Key::Toggle);
        let rows = st.render(80, false);
        assert!(rows[0].contains("q"), "question row first");
        assert!(rows[1].starts_with("> [x] O0"), "got {:?}", rows[1]);
        assert!(rows[2].starts_with("  [ ] O1"), "got {:?}", rows[2]);
        // The reply row sits with the options, directly above the hint, and
        // wears the same `caret marker text` shape they do.
        assert!(
            rows.last().unwrap().contains("space/tab toggles"),
            "{rows:?}"
        );
        assert_eq!(reply_row(&rows).trim_end(), "  { } type to reply");
        for row in &rows {
            assert!(row.is_ascii(), "no non-ascii in {row:?}");
        }
        // Single-select uses radio markers.
        let st = state("one", 2);
        let rows = st.render(80, false);
        assert!(rows[1].starts_with("> (*) O0"), "got {:?}", rows[1]);
        assert!(rows[2].starts_with("  ( ) O1"), "got {:?}", rows[2]);
    }

    #[test]
    fn short_terminals_scroll_instead_of_overflowing() {
        let mut st = state("one", 10);
        st.set_window(3);
        assert_eq!(
            st.height(),
            CHROME_ROWS + 3 + 1,
            "window + question + hint + reply + more-row"
        );
        for _ in 0..5 {
            st.apply(Key::Down);
        }
        let rows = st.render(80, false);
        // Never more rows than the window plus chrome.
        assert!(rows.len() <= 3 + CHROME_ROWS + 2, "got {} rows", rows.len());
        assert!(
            rows.iter().any(|r| r.contains("above")),
            "scrolled window must say what is off-screen: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("O5")),
            "cursor must be visible"
        );
    }

    #[test]
    fn popup_budget_degrades_and_never_touches_the_footer() {
        assert_eq!(popup_budget(0), 0, "no body rows: inline fallback");
        assert_eq!(popup_budget(3), 0);
        assert_eq!(popup_budget(4), 3, "clamped to body - 1");
        assert!(popup_budget(24) < 24, "popup never eats the whole body");
        for body in 0u16..60 {
            let b = popup_budget(body);
            assert!(
                b == 0 || b < body as usize,
                "budget {b} must leave body rows at {body}"
            );
            assert_eq!(option_window(body), b.saturating_sub(CHROME_ROWS));
        }
        let st = state("one", 3);
        assert!(st.fits_inline_only(2));
        assert!(!st.fits_inline_only(24));
    }

    #[test]
    fn body_rows_leave_the_tray_its_footer_rows() {
        // A window with two pinned banners still reports a body that stops
        // above the footer, so the popup cannot paint over the tray.
        let rows = 40u16;
        let body = body_rows_for(rows, crate::escalation::ROWS_PER_BANNER * 2);
        assert!(
            body < rows,
            "body must stop above the footer: {body} of {rows}"
        );
        let plan =
            crate::terminal::FooterLayout::solve(rows, crate::escalation::ROWS_PER_BANNER * 2);
        assert_eq!(body, plan.body_bottom);
        assert!(
            plan.banner_rows > 0,
            "tray rows must survive alongside the popup"
        );
    }

    #[test]
    fn answers_are_valid_json_and_escape_hostile_ids() {
        assert_eq!(
            answer_json(ChoiceMode::One, &["a\"b".to_string()]),
            "{\"choice\":\"a\\\"b\"}"
        );
        assert_eq!(
            answer_json(ChoiceMode::Many, &["a".to_string(), "b".to_string()]),
            "{\"choice\":[\"a\",\"b\"]}"
        );
        assert_eq!(answer_json(ChoiceMode::Many, &[]), "{\"choice\":[]}");
    }
}
