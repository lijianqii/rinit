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

use crate::unit::UnitName;

/// Default total buffer size: 256 KiB.
///
/// Comfortably inside the 128 MB budget, and large enough to hold the boot log
/// of a system with a few dozen services.
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024;

/// Longest prefix of one line that is kept (1 KiB).
pub const DEFAULT_MAX_LINE_BYTES: usize = 1024;

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
    pub fn push(&mut self, unit: &UnitName, text: &str) {
        // A zero-size buffer stores nothing; without this it would accumulate
        // empty lines forever.
        if self.max_bytes == 0 {
            return;
        }
        // Truncate to the smaller bound, so one line can never exceed the whole
        // buffer (which would leave it permanently over budget).
        let text = truncate(text, self.max_line_bytes.min(self.max_bytes));
        self.bytes += text.len();
        self.lines.push_back(LogLine {
            unit: unit.clone(),
            text: text.to_string(),
        });
        // A single line always fits now, so this can never empty the buffer.
        while self.bytes > self.max_bytes {
            match self.lines.pop_front() {
                Some(dropped) => self.bytes -= dropped.text.len(),
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

    /// How many bytes of text the buffer currently holds.
    pub fn bytes(&self) -> usize {
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

    /// The bound is on bytes: pushing past it drops the oldest lines.
    #[test]
    fn the_buffer_drops_the_oldest_lines_when_full() {
        // Room for three 4-byte lines.
        let mut buffer = LogBuffer::new(12, 64);
        let a = unit("a.service");
        for text in ["aaaa", "bbbb", "cccc", "dddd"] {
            buffer.push(&a, text);
        }
        assert_eq!(buffer.bytes(), 12);
        let tail = buffer.tail(10);
        let texts: Vec<&str> = tail.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["bbbb", "cccc", "dddd"], "the oldest line is gone");
    }

    /// A single long line is truncated, so it cannot evict the whole buffer.
    #[test]
    fn a_long_line_is_truncated() {
        let mut buffer = LogBuffer::new(1024, 8);
        let a = unit("a.service");
        buffer.push(&a, "0123456789");

        assert_eq!(buffer.tail(1)[0].text, "01234567");
        assert_eq!(buffer.bytes(), 8);
    }

    /// Truncation must not split a multi-byte character.
    #[test]
    fn truncation_respects_char_boundaries() {
        let mut buffer = LogBuffer::new(1024, 5);
        let a = unit("a.service");
        // "é" is two bytes, so 5 bytes would cut it in half.
        buffer.push(&a, "aééé");

        let text = &buffer.tail(1)[0].text;
        assert!(text.len() <= 5, "{text:?} is {} bytes", text.len());
        // The result is still valid UTF-8 by construction; assert it ends on
        // the last whole character.
        assert_eq!(text, "aéé");
    }

    /// A line longer than the whole buffer is truncated to fit, rather than
    /// leaving the buffer permanently over budget.
    #[test]
    fn a_line_larger_than_the_buffer_is_truncated_to_it() {
        let mut buffer = LogBuffer::new(4, 1024);
        let a = unit("a.service");
        buffer.push(&a, "abcdefgh");
        assert_eq!(buffer.len(), 1);
        assert!(buffer.bytes() <= 4, "bytes = {}", buffer.bytes());
        assert_eq!(buffer.tail(1)[0].text, "abcd");
    }

    /// A zero-sized buffer must not accumulate empty lines.
    #[test]
    fn a_zero_sized_buffer_stores_nothing() {
        let mut buffer = LogBuffer::new(0, 1024);
        let a = unit("a.service");
        buffer.push(&a, "anything");
        assert!(buffer.is_empty());
        assert_eq!(buffer.bytes(), 0);
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
