//! The service log buffer (plan §1.3(7)).
//!
//! One global ring, shared by every unit, rather than a buffer per service:
//! the plan picks that shape for "more than 20 services", which is exactly this
//! project's target, and it bounds rinit's log memory with a single constant
//! instead of `services × per-service cap`.
//!
//! Bounded by *bytes*, not lines, because a service that prints one enormous
//! line must not be able to hold more memory than one that prints many short
//! ones. The oldest lines are dropped first, and each line is truncated so a
//! single pathological line cannot evict the whole buffer.
//!
//! Pure and `no_std` + alloc, so the eviction and truncation rules - the part
//! that decides how much memory rinit can be made to hold - are unit-tested.

use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::mem::size_of;

use crate::unit::UnitName;

/// Default total buffer size: 256 KiB.
///
/// Comfortably inside the 128 MB budget, and large enough to hold the boot log
/// of a system with a few dozen services.
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024;

/// Longest prefix of one line that is kept (1 KiB).
pub const DEFAULT_MAX_LINE_BYTES: usize = 1024;

/// What one buffered line costs beyond its own text.
///
/// [`LogBuffer`]'s budget is expressed in bytes, and a budget that counted only
/// text would not bound memory: 256 KiB of three-byte lines is 87,000
/// `LogLine`s, which occupies about 11 MB once each line's struct, its slot in
/// the deque and the unit name it carries are paid for - 40x what the budget
/// claims, on a 128 MB target that is sized against that number.
///
/// Charging the fixed part of a line keeps the real footprint within a small
/// factor of `max_bytes`. Measured (`tests/allocations.rs`), a budget of
/// `DEFAULT_MAX_BYTES` now holds well under a megabyte for any line length.
pub const LINE_OVERHEAD: usize = 2 * size_of::<LogLine>();

/// The bytes one stored line occupies: its text, its unit name and
/// [`LINE_OVERHEAD`].
fn line_cost(unit: &UnitName, text: &str) -> usize {
    text.len() + unit.canonical().len() + LINE_OVERHEAD
}

/// One captured line, tagged with the unit that wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// The unit whose stdout/stderr produced it.
    pub unit: UnitName,
    /// The line itself, without a trailing newline.
    pub text: String,
}

/// A bounded, oldest-first log ring shared by every unit.
#[derive(Debug)]
pub struct LogBuffer {
    lines: VecDeque<LogLine>,
    bytes: usize,
    max_bytes: usize,
    max_line_bytes: usize,
}

impl LogBuffer {
    /// An empty buffer with explicit bounds.
    pub const fn new(max_bytes: usize, max_line_bytes: usize) -> Self {
        LogBuffer {
            lines: VecDeque::new(),
            bytes: 0,
            max_bytes,
            max_line_bytes,
        }
    }

    /// An empty buffer with the defaults for this project.
    pub fn with_defaults() -> Self {
        LogBuffer::new(DEFAULT_MAX_BYTES, DEFAULT_MAX_LINE_BYTES)
    }

    /// Append one line, evicting the oldest until the buffer fits again.
    ///
    /// The budget counts the whole line, not just its text (see
    /// [`LINE_OVERHEAD`]), so "the buffer holds at most `max_bytes`" is a
    /// statement about memory rather than about the sum of a few strings.
    pub fn push(&mut self, unit: &UnitName, text: &str) {
        // A zero-size buffer stores nothing; without this it would accumulate
        // empty lines forever.
        if self.max_bytes == 0 {
            return;
        }
        // Truncate to the smaller of the line bound and what the budget has
        // left for text once this line's tag and overhead are paid for. That
        // way the line being pushed always fits, so it can never evict itself.
        let room = self
            .max_bytes
            .saturating_sub(unit.canonical().len() + LINE_OVERHEAD);
        let text = truncate(text, self.max_line_bytes.min(room));
        self.bytes += line_cost(unit, text);
        self.lines.push_back(LogLine {
            unit: unit.clone(),
            text: text.to_string(),
        });
        while self.bytes > self.max_bytes {
            match self.lines.pop_front() {
                Some(dropped) => self.bytes -= line_cost(&dropped.unit, &dropped.text),
                None => break,
            }
        }
    }

    /// The most recent `count` lines from every unit, oldest first.
    pub fn tail(&self, count: usize) -> Vec<LogLine> {
        let skip = self.lines.len().saturating_sub(count);
        self.lines.iter().skip(skip).cloned().collect()
    }

    /// The most recent `count` lines from `unit`, oldest first.
    pub fn tail_for(&self, unit: &UnitName, count: usize) -> Vec<LogLine> {
        let matching: Vec<&LogLine> = self
            .lines
            .iter()
            .filter(|line| &line.unit == unit)
            .collect();
        let skip = matching.len().saturating_sub(count);
        matching.into_iter().skip(skip).cloned().collect()
    }

    /// How many lines are buffered.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether nothing has been captured.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// How many bytes of buffered lines are accounted for: every line's text,
    /// its unit name and its [`LINE_OVERHEAD`].
    ///
    /// This is the number [`LogBuffer::new`]'s budget is compared against, so
    /// it tracks the buffer's real memory rather than the length of the strings
    /// it happens to hold.
    pub fn accounted_bytes(&self) -> usize {
        self.bytes
    }

    /// Forget everything (used when a unit is removed).
    pub fn clear(&mut self) {
        self.lines.clear();
        self.bytes = 0;
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        LogBuffer::with_defaults()
    }
}

/// The longest prefix of `text` that is at most `max` bytes and ends on a char
/// boundary.
fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(name: &str) -> UnitName {
        UnitName::parse(name).expect("valid name")
    }

    #[test]
    fn lines_come_back_oldest_first() {
        let mut buffer = LogBuffer::with_defaults();
        let a = unit("a.service");
        buffer.push(&a, "one");
        buffer.push(&a, "two");
        buffer.push(&a, "three");

        let lines = buffer.tail(2);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "two");
        assert_eq!(lines[1].text, "three");
    }

    /// The bound counts whole lines, so this is the budget for `count` lines of
    /// `text` written by `unit`.
    fn budget_for(unit: &UnitName, text: &str, count: usize) -> usize {
        count * line_cost(unit, text)
    }

    /// The bound is on bytes: pushing past it drops the oldest lines.
    #[test]
    fn the_buffer_drops_the_oldest_lines_when_full() {
        let a = unit("a.service");
        // Room for exactly three 4-byte lines from this unit.
        let mut buffer = LogBuffer::new(budget_for(&a, "aaaa", 3), 64);
        for text in ["aaaa", "bbbb", "cccc", "dddd"] {
            buffer.push(&a, text);
        }
        assert_eq!(buffer.accounted_bytes(), budget_for(&a, "aaaa", 3));
        let tail = buffer.tail(10);
        let texts: Vec<&str> = tail.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["bbbb", "cccc", "dddd"], "the oldest line is gone");
    }

    /// The budget is a memory bound, not a text-length bound: short lines must
    /// not buy thousands of `LogLine` structs.
    ///
    /// Without [`LINE_OVERHEAD`] in the accounting, 256 KiB of three-byte lines
    /// is 87,000 lines, and the buffer grows to about 11 MB - which is exactly
    /// what the soak test caught.
    #[test]
    fn short_lines_cannot_explode_the_footprint() {
        let a = unit("a.service");
        let mut buffer = LogBuffer::with_defaults();
        for _ in 0..200_000 {
            buffer.push(&a, "hi");
        }

        assert!(
            buffer.accounted_bytes() <= DEFAULT_MAX_BYTES,
            "accounted {} > budget {DEFAULT_MAX_BYTES}",
            buffer.accounted_bytes()
        );
        let per_line = line_cost(&a, "hi");
        assert!(
            buffer.len() <= DEFAULT_MAX_BYTES / per_line,
            "{} lines is more than the budget can pay for",
            buffer.len()
        );
    }

    /// A single long line is truncated, so it cannot evict the whole buffer.
    #[test]
    fn a_long_line_is_truncated() {
        let a = unit("a.service");
        let mut buffer = LogBuffer::new(4096, 8);
        buffer.push(&a, "0123456789");

        assert_eq!(buffer.tail(1)[0].text, "01234567");
        assert_eq!(buffer.accounted_bytes(), line_cost(&a, "01234567"));
    }

    /// A budget too small to pay for even one line's overhead stores nothing,
    /// rather than storing one line and calling it "within budget".
    #[test]
    fn a_budget_smaller_than_one_line_stores_nothing() {
        let a = unit("a.service");
        let mut buffer = LogBuffer::new(4, 1024);
        buffer.push(&a, "abcdefgh");

        assert!(buffer.is_empty(), "no line fits in 4 accounted bytes");
        assert_eq!(buffer.accounted_bytes(), 0);
    }

    /// The newest line is always kept: it is truncated to the budget before it
    /// is stored, so it can never be the line that evicts itself.
    #[test]
    fn the_newest_line_survives_even_when_it_fills_the_budget() {
        let a = unit("a.service");
        let mut buffer = LogBuffer::new(1024, 4096);
        buffer.push(&a, "first");
        buffer.push(&a, &"x".repeat(4096));

        let tail = buffer.tail(10);
        assert_eq!(tail.len(), 1, "the older line is evicted, not the new one");
        assert!(tail[0].text.starts_with("xxx"));
        assert!(buffer.accounted_bytes() <= 1024);
    }

    /// Truncation must not split a multi-byte character.
    #[test]
    fn truncation_respects_char_boundaries() {
        let a = unit("a.service");
        let mut buffer = LogBuffer::new(1024, 5);
        // "é" is two bytes, so 5 bytes would cut it in half.
        buffer.push(&a, "aééé");

        let text = &buffer.tail(1)[0].text;
        assert!(text.len() <= 5, "{text:?} is {} bytes", text.len());
        // The result is still valid UTF-8 by construction; assert it ends on
        // the last whole character.
        assert_eq!(text, "aéé");
    }

    /// A zero-sized buffer must not accumulate empty lines.
    #[test]
    fn a_zero_sized_buffer_stores_nothing() {
        let mut buffer = LogBuffer::new(0, 1024);
        let a = unit("a.service");
        buffer.push(&a, "anything");
        assert!(buffer.is_empty());
        assert_eq!(buffer.accounted_bytes(), 0);
    }

    #[test]
    fn tail_can_be_filtered_by_unit() {
        let mut buffer = LogBuffer::with_defaults();
        let a = unit("a.service");
        let b = unit("b.service");
        buffer.push(&a, "a1");
        buffer.push(&b, "b1");
        buffer.push(&a, "a2");

        let a_tail = buffer.tail_for(&a, 10);
        let a_lines: Vec<&str> = a_tail.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(a_lines, ["a1", "a2"]);
        let b_tail = buffer.tail_for(&b, 10);
        let b_lines: Vec<&str> = b_tail.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(b_lines, ["b1"]);
        assert!(buffer.tail_for(&unit("c.service"), 10).is_empty());
    }

    /// Asking for more lines than exist returns what there is, not an error.
    #[test]
    fn tail_is_saturating() {
        let mut buffer = LogBuffer::with_defaults();
        buffer.push(&unit("a.service"), "only");
        assert_eq!(buffer.tail(100).len(), 1);
        assert_eq!(buffer.tail(0).len(), 0);
    }
}
