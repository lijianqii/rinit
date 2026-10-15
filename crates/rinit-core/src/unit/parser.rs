//! Unit-file parser (plan §1.3(1), §1.0 EBNF, §1.1 contract).
//!
//! A pure function: text in, [`UnitConfig`] out, no global state and no I/O.
//! That is what makes it unit-testable in this crate, fuzzable per plan §4.4,
//! and reusable from the fake-runtime integration tests of plan §4.2.
//!
//! The lexer implements the EBNF in §1.0 exactly, because those rules exist to
//! make the grammar unambiguous under fuzzing:
//!
//! - a bare `#` *always* starts a comment, anywhere outside quotes, so a
//!   literal `#` in a value must be escaped (`\#`) or quoted;
//! - `\` is an escape everywhere; `\s` is a literal space;
//! - a line ending in `\` continues onto the next line;
//! - `key = value` splits at the first `=`, whitespace around the key ignored.
//!
//! Values lex into *tokens* rather than one string, because the two consumers
//! need different shapes: a list directive's tokens are its items, and a
//! command directive's tokens are its `argv`. Joining first would lose the
//! quote boundaries `ExecStart=/bin/echo "hello world"` depends on.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::config::{
    Cap, CpuQuota, ExecCommand, RestartPolicy, ServiceKind, SizeBytes, UnitConfig,
};
use super::{UnitName, UnitType};

/// A parse failure, always tied to a file position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// A line that is neither a section header, an entry, nor a comment.
    Syntax {
        /// 1-based line number.
        line: usize,
        /// What was expected there.
        message: String,
    },
    /// A `[Section]` this format does not define.
    UnknownSection {
        /// 1-based line number.
        line: usize,
        /// The section name as written.
        section: String,
    },
    /// A `Key` not defined for the section it appears in.
    UnknownKey {
        /// 1-based line number.
        line: usize,
        /// The section the key appeared in.
        section: &'static str,
        /// The key as written.
        key: String,
    },
    /// The same key twice in one section; the later value would silently win.
    DuplicateKey {
        /// 1-based line number of the repeat.
        line: usize,
        /// The repeated key.
        key: String,
    },
    /// An entry outside any `[Unit]` / `[Service]` header.
    EntryOutsideSection {
        /// 1-based line number.
        line: usize,
        /// The offending key.
        key: String,
    },
    /// A value that does not satisfy its directive's contract.
    InvalidValue {
        /// 1-based line number.
        line: usize,
        /// The key being parsed.
        key: String,
        /// The value as written.
        value: String,
        /// Why it was rejected.
        reason: String,
    },
    /// A quoted string that is never closed.
    UnterminatedQuote {
        /// 1-based line number.
        line: usize,
    },
    /// A service with no `ExecStart=`.
    MissingExecStart {
        /// The unit that has no command.
        unit: String,
    },
    /// `[Unit] Type=` contradicting the file's suffix (plan §1.1).
    TypeSuffixConflict {
        /// The unit name.
        unit: String,
        /// The value declared in the file.
        declared: &'static str,
        /// The type implied by the suffix.
        implied: &'static str,
    },
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ParseError::Syntax { line, message } => write!(f, "line {line}: {message}"),
            ParseError::UnknownSection { line, section } => {
                write!(f, "line {line}: unknown section [{section}]")
            }
            ParseError::UnknownKey { line, section, key } => {
                write!(f, "line {line}: unknown key '{key}' in [{section}]")
            }
            ParseError::DuplicateKey { line, key } => {
                write!(f, "line {line}: duplicate key '{key}'")
            }
            ParseError::EntryOutsideSection { line, key } => {
                write!(f, "line {line}: '{key}' appears before any section header")
            }
            ParseError::InvalidValue {
                line,
                key,
                value,
                reason,
            } => {
                write!(
                    f,
                    "line {line}: invalid value for '{key}': {value:?} ({reason})"
                )
            }
            ParseError::UnterminatedQuote { line } => {
                write!(f, "line {line}: unterminated quoted string")
            }
            ParseError::MissingExecStart { unit } => {
                write!(f, "{unit}: a service must define ExecStart=")
            }
            ParseError::TypeSuffixConflict {
                unit,
                declared,
                implied,
            } => write!(
                f,
                "{unit}: [Unit] Type={declared} contradicts the file suffix ({implied})"
            ),
        }
    }
}

/// Parse a unit file.
///
/// `name` comes from the file name and supplies both the unit's identity and
/// the suffix that `[Unit] Type=` must agree with.
///
/// # Errors
/// Returns [`ParseError`] for anything the §1.0 grammar or the §1.1 contract
/// rejects. Nothing is inferred: an unknown key or an unparsable value is an
/// error, because a typo in a unit file is otherwise a silent no-op.
pub fn parse(name: &UnitName, input: &str) -> Result<UnitConfig, ParseError> {
    Parser::new(name).run(input)
}

/// Parse text for a unit identified by name.
pub fn parse_named(name: &str, input: &str) -> Result<UnitConfig, crate::CoreError> {
    let unit = UnitName::parse(name)?;
    Ok(parse(&unit, input)?)
}

/// A section of the file being parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Unit,
    Service,
}

impl Section {
    const fn as_str(self) -> &'static str {
        match self {
            Section::Unit => "Unit",
            Section::Service => "Service",
        }
    }
}

/// Whether a numeric directive may take the value 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Zero {
    /// Zero is meaningful for this key, and plan §1.1 spells out what it means:
    /// "restart immediately" (`RestartSec`), "no timeout" (`TimeoutStartSec`),
    /// "SIGKILL at once" (`TimeoutStopSec`), "disable the breaker"
    /// (`StartLimitIntervalSec`).
    Allowed,
    /// Zero is meaningless here. Plan §1.1: "其余数值配置项不接受 0(报
    /// `InvalidValue`)".
    Rejected,
}

struct Parser {
    name: UnitName,
    cfg: UnitConfig,
    declared_unit_type: Option<UnitType>,
    current_section: Option<Section>,
    /// `Section::key` pairs already seen, so the two `Type` directives (which
    /// legitimately appear once per section) do not collide.
    seen: Vec<(&'static str, String)>,
}

/// Directives that may legally appear more than once.
///
/// Plan §1.1 types the dependency directives and `Environment` as vectors, so
/// repeating them accumulates. This also matches how operators actually write
/// units: a long `Requires=` list split across several lines is far more
/// readable than one 200-character line.
///
/// Everything else is single-valued. A repeat there is a typo, and silently
/// keeping the first or last value is exactly the class of bug this parser
/// exists to prevent.
const REPEATABLE: &[&str] = &[
    "Environment",
    "Requires",
    "Wants",
    "After",
    "Before",
    "Conflicts",
    "ExecStartPre",
    "ExecStartPost",
];

impl Parser {
    fn new(name: &UnitName) -> Self {
        Parser {
            name: name.clone(),
            cfg: UnitConfig::new(name.clone()),
            declared_unit_type: None,
            current_section: None,
            seen: Vec::new(),
        }
    }

    fn run(mut self, input: &str) -> Result<UnitConfig, ParseError> {
        for (line_no, line) in logical_lines(input) {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some(section) = section_header(trimmed, line_no)? {
                self.current_section = Some(section);
                continue;
            }
            let Some(section) = self.current_section else {
                return Err(ParseError::EntryOutsideSection {
                    line: line_no,
                    key: trimmed
                        .split('=')
                        .next()
                        .unwrap_or(trimmed)
                        .trim()
                        .to_string(),
                });
            };
            self.entry(section, line_no, trimmed)?;
        }
        self.finish()
    }

    /// Validate the assembled config once the whole file has been read.
    fn finish(mut self) -> Result<UnitConfig, ParseError> {
        // Plan §1.1: a suffix/`Type=` disagreement is rejected, never silently
        // resolved. An absent `Type=` is not a disagreement, so the suffix
        // decides and `multi-user.target` needs no `Type=` at all.
        if let Some(declared) = self.declared_unit_type
            && declared != self.name.unit_type()
        {
            return Err(ParseError::TypeSuffixConflict {
                unit: self.name.canonical().to_string(),
                declared: match declared {
                    UnitType::Service => "service",
                    UnitType::Target => "target",
                },
                implied: match self.name.unit_type() {
                    UnitType::Service => "service",
                    UnitType::Target => "target",
                },
            });
        }
        self.cfg.kind = self.name.unit_type();

        // Targets are process-less aggregation points (plan §7.2), so they are
        // exempt from the command requirement.
        if !self.cfg.is_target() && self.cfg.exec_start.is_empty() {
            return Err(ParseError::MissingExecStart {
                unit: self.name.canonical().to_string(),
            });
        }
        Ok(self.cfg)
    }

    fn entry(&mut self, section: Section, line: usize, text: &str) -> Result<(), ParseError> {
        let (key, value) = split_entry(text).ok_or_else(|| ParseError::Syntax {
            line,
            message: format!("expected 'Key=Value', found {text:?}"),
        })?;

        if !REPEATABLE.contains(&key)
            && self
                .seen
                .iter()
                .any(|(s, k)| *s == section.as_str() && k == key)
        {
            return Err(ParseError::DuplicateKey {
                line,
                key: key.to_string(),
            });
        }
        self.seen.push((section.as_str(), key.to_string()));

        let tokens = tokenize(value, line)?;
        let joined = || join_tokens(&tokens);

        match (section, key) {
            // --- [Unit] ---------------------------------------------------
            (Section::Unit, "Description") => self.cfg.description = joined(),
            (Section::Unit, "Documentation") => self.cfg.documentation = joined(),
            (Section::Unit, "Type") => {
                let declared = joined();
                self.declared_unit_type = Some(match declared.as_str() {
                    "service" => UnitType::Service,
                    "target" => UnitType::Target,
                    _ => {
                        return Err(invalid(
                            line,
                            key,
                            &declared,
                            "expected 'service' or 'target'",
                        ));
                    }
                });
            }
            (Section::Unit, "Requires") => self.unit_list(line, key, &tokens, Slot::Requires)?,
            (Section::Unit, "Wants") => self.unit_list(line, key, &tokens, Slot::Wants)?,
            (Section::Unit, "After") => self.unit_list(line, key, &tokens, Slot::After)?,
            (Section::Unit, "Before") => self.unit_list(line, key, &tokens, Slot::Before)?,
            (Section::Unit, "Conflicts") => self.unit_list(line, key, &tokens, Slot::Conflicts)?,

            // --- [Service] ------------------------------------------------
            (Section::Service, "Type") => {
                let declared = joined();
                self.cfg.service_kind = ServiceKind::parse(&declared).ok_or_else(|| {
                    invalid(
                        line,
                        key,
                        &declared,
                        "expected 'simple', 'oneshot' or 'notify'",
                    )
                })?;
            }
            (Section::Service, "ExecStart") => {
                // An empty value leaves the command unset; `finish` then reports
                // `MissingExecStart`, which names the unit (plan §1.1). Rejecting
                // it here as `InvalidValue` would contradict the contract.
                self.cfg.exec_start = command(&tokens);
            }
            (Section::Service, "ExecStop") => {
                // `ExecStop=` (String, default empty) is legal and means "send
                // SIGTERM" (plan §1.1).
                self.cfg.exec_stop = command(&tokens);
            }
            (Section::Service, "ExecStartPre") => {
                let cmd = command(&tokens);
                if !cmd.is_empty() {
                    self.cfg.exec_start_pre.push(cmd);
                }
            }
            (Section::Service, "ExecStartPost") => {
                let cmd = command(&tokens);
                if !cmd.is_empty() {
                    self.cfg.exec_start_post.push(cmd);
                }
            }
            (Section::Service, "HealthCheck") => {
                // `HealthCheck=` empty means "no health checking" (plan §1.1).
                self.cfg.health_check = command(&tokens);
            }
            (Section::Service, "Restart") => {
                let declared = joined();
                self.cfg.restart = RestartPolicy::parse(&declared).ok_or_else(|| {
                    invalid(
                        line,
                        key,
                        &declared,
                        "expected 'no', 'always', 'on-success', 'on-failure' or 'on-abnormal'",
                    )
                })?;
            }
            (Section::Service, "RestartSec") => {
                self.cfg.restart_sec = number(line, key, &tokens, Zero::Allowed)?;
            }
            (Section::Service, "StartLimitBurst") => {
                self.cfg.start_limit_burst = number(line, key, &tokens, Zero::Rejected)?;
            }
            (Section::Service, "StartLimitIntervalSec") => {
                self.cfg.start_limit_interval_sec = number(line, key, &tokens, Zero::Allowed)?;
            }
            (Section::Service, "TimeoutStartSec") => {
                self.cfg.timeout_start_sec = number(line, key, &tokens, Zero::Allowed)?;
            }
            (Section::Service, "TimeoutStopSec") => {
                self.cfg.timeout_stop_sec = number(line, key, &tokens, Zero::Allowed)?;
            }
            (Section::Service, "HealthIntervalSec") => {
                self.cfg.health_interval_sec = number(line, key, &tokens, Zero::Rejected)?;
            }
            (Section::Service, "HealthTimeoutSec") => {
                self.cfg.health_timeout_sec = number(line, key, &tokens, Zero::Rejected)?;
            }
            (Section::Service, "User") => self.cfg.user = joined(),
            (Section::Service, "Group") => self.cfg.group = joined(),
            (Section::Service, "NoNewPrivileges") => {
                self.cfg.no_new_privileges = boolean(line, key, &tokens)?;
            }
            (Section::Service, "HealthRestartOnFailure") => {
                self.cfg.health_restart_on_failure = boolean(line, key, &tokens)?;
            }
            (Section::Service, "CapabilityBoundingSet") => {
                // An explicitly empty value clears the set, which is why this
                // is `Some(vec![])` rather than `None`.
                self.cfg.capability_bounding_set = Some(caps(line, key, &tokens)?);
            }
            (Section::Service, "AmbientCapabilities") => {
                self.cfg.ambient_capabilities = Some(caps(line, key, &tokens)?);
            }
            (Section::Service, "Environment") => {
                let extra = environment(line, key, &tokens)?;
                self.cfg.environment.extend(extra);
            }
            (Section::Service, "EnvironmentFile") => {
                // Like `WorkingDirectory`, an empty value is "unset", not a
                // relative path; only a non-empty value must be absolute.
                if !tokens.is_empty() {
                    let path = joined();
                    require_absolute(line, key, &path)?;
                    self.cfg.environment_file = path;
                }
            }
            (Section::Service, "WorkingDirectory") => {
                // An empty value keeps the `/` default (plan §1.1).
                if !tokens.is_empty() {
                    let path = joined();
                    require_absolute(line, key, &path)?;
                    self.cfg.working_directory = path;
                }
            }
            (Section::Service, "CgroupMemoryMax") => {
                let text = joined();
                self.cfg.cgroup_memory_max = Some(size(line, key, &text)?);
            }
            (Section::Service, "CgroupMemoryHigh") => {
                let text = joined();
                self.cfg.cgroup_memory_high = Some(size(line, key, &text)?);
            }
            (Section::Service, "CgroupCPUMax") => {
                let text = joined();
                self.cfg.cgroup_cpu_max = Some(CpuQuota::parse(&text).ok_or_else(|| {
                    invalid(line, key, &text, "expected '50%' or 'quota period'")
                })?);
            }
            (Section::Service, "CgroupTasksMax") => {
                self.cfg.cgroup_tasks_max = Some(number(line, key, &tokens, Zero::Rejected)?);
            }

            (_, other) => {
                return Err(ParseError::UnknownKey {
                    line,
                    section: section.as_str(),
                    key: other.to_string(),
                });
            }
        }
        Ok(())
    }

    fn unit_list(
        &mut self,
        line: usize,
        key: &str,
        tokens: &[String],
        slot: Slot,
    ) -> Result<(), ParseError> {
        let mut parsed = Vec::with_capacity(tokens.len());
        for token in tokens {
            // Plan §1.1: a bare name defaults to `.service`.
            let unit =
                UnitName::parse(token).map_err(|e| invalid(line, key, token, &e.to_string()))?;
            parsed.push(unit);
        }
        let target = match slot {
            Slot::Requires => &mut self.cfg.deps.requires,
            Slot::Wants => &mut self.cfg.deps.wants,
            Slot::After => &mut self.cfg.deps.after,
            Slot::Before => &mut self.cfg.deps.before,
            Slot::Conflicts => &mut self.cfg.deps.conflicts,
        };
        target.extend(parsed);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum Slot {
    Requires,
    Wants,
    After,
    Before,
    Conflicts,
}

/// Split the file into logical lines, joining `\`-continued ones.
///
/// The reported number is the line the *statement* started on, so an error in
/// a continuation points at the directive the operator actually wrote.
fn logical_lines(input: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut current = String::new();
    let mut start = 0usize;

    for (idx, raw) in input.lines().enumerate() {
        // The EBNF allows CRLF.
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let line_no = idx + 1;

        let continuing = !line.trim_start().starts_with('#')
            && line.trim_end().ends_with('\\')
            && !line
                .trim_end()
                .strip_suffix('\\')
                .unwrap_or("")
                .trim()
                .is_empty();

        if current.is_empty() {
            start = line_no;
        }
        if continuing {
            let head = line.trim_end().strip_suffix('\\').unwrap_or(line);
            current.push_str(head);
            // Join with a single space so `Description=a \` + `b` is `a b`.
            current.push(' ');
        } else {
            current.push_str(line);
            out.push((start, core::mem::take(&mut current)));
        }
    }
    if !current.is_empty() {
        out.push((start, current));
    }
    out
}

/// Recognise a `[Section]` header, rejecting trailing content.
fn section_header(line: &str, line_no: usize) -> Result<Option<Section>, ParseError> {
    if !line.starts_with('[') {
        return Ok(None);
    }
    let rest = &line[1..];
    let (name, tail) = rest.split_once(']').ok_or(ParseError::Syntax {
        line: line_no,
        message: "missing ']' in section header".to_string(),
    })?;
    // The EBNF puts `eol` straight after `]`.
    if !tail.trim().is_empty() {
        return Err(ParseError::Syntax {
            line: line_no,
            message: format!("unexpected content after section header: {tail:?}"),
        });
    }
    match name.trim() {
        "Unit" => Ok(Some(Section::Unit)),
        "Service" => Ok(Some(Section::Service)),
        other => Err(ParseError::UnknownSection {
            line: line_no,
            section: other.to_string(),
        }),
    }
}

/// Split `key = value` at the first `=`, trimming whitespace around the key.
///
/// Returns `None` for anything that is not `ALPHA (ALPHA|DIGIT)* = ...`, which
/// the caller reports as a syntax error.
fn split_entry(text: &str) -> Option<(&str, &str)> {
    let (key, value) = text.split_once('=')?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    let mut bytes = key.bytes();
    if !bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        || !bytes.all(|b| b.is_ascii_alphanumeric())
    {
        return None;
    }
    Some((key, value.trim()))
}

/// Lex a value into tokens, applying the §1.0 quoting and escaping rules.
///
/// A bare `#` outside quotes ends the value wherever it appears, exactly as the
/// EBNF's `unquoted-char` excludes it; `\#` is how a literal one is written.
fn tokenize(value: &str, line: usize) -> Result<Vec<String>, ParseError> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut chars = value.chars();

    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if started {
                    out.push(core::mem::take(&mut cur));
                    started = false;
                }
            }
            '#' => break, // comment to end of line
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => cur.push_str(&escape(&mut chars)),
                        Some(other) => cur.push(other),
                        None => return Err(ParseError::UnterminatedQuote { line }),
                    }
                }
            }
            '\\' => {
                started = true;
                cur.push_str(&escape(&mut chars));
            }
            other => {
                started = true;
                cur.push(other);
            }
        }
    }
    if started {
        out.push(cur);
    }
    Ok(out)
}

/// Consume the character after a backslash and return its replacement.
///
/// The EBNF names `\\ \" n t s`; anything else yields the character itself, so
/// an unknown escape is a literal rather than an error.
fn escape(chars: &mut core::str::Chars<'_>) -> String {
    let decoded = match chars.next() {
        Some('n') => "\n",
        Some('t') => "\t",
        Some('s') => " ",
        Some('\\') => "\\",
        Some('"') => "\"",
        None => "\\", // a trailing backslash: keep it literal
        Some(other) => {
            let mut buf = [0u8; 4];
            return other.encode_utf8(&mut buf).to_string();
        }
    };
    decoded.to_string()
}

/// Join tokens back into one string, collapsing internal whitespace.
fn join_tokens(tokens: &[String]) -> String {
    let mut out = String::new();
    for (i, t) in tokens.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(t);
    }
    out
}

/// Parse the body of an `EnvironmentFile=` into `KEY=VALUE` pairs (plan §1.1).
///
/// One entry per line. A bare `#` starts a comment and blank lines are ignored.
/// There is deliberately no continuation, no quoting and no variable expansion:
/// unlike a unit file, this format is the minimal "one variable per line" that
/// every distribution ships, and a line that is not `KEY=VALUE` is skipped
/// rather than failing the boot.
pub fn parse_environment_file(contents: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in contents.lines() {
        // A bare `#` ends the line, exactly as in a unit file (plan §1.0).
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        out.push((key.to_string(), value.trim().to_string()));
    }
    out
}

fn invalid(line: usize, key: &str, value: &str, reason: &str) -> ParseError {
    ParseError::InvalidValue {
        line,
        key: key.to_string(),
        value: value.to_string(),
        reason: reason.to_string(),
    }
}

/// Build a command from its tokenised value.
///
/// An empty value (or one that tokenises to nothing, e.g. `""`) yields an
/// *unset* command. Each caller interprets that per the contract (plan §1.1):
/// `ExecStart` becomes `MissingExecStart` in `finish`, `ExecStop`/
/// `HealthCheck` mean "empty", and `ExecStartPre`/`ExecStartPost` contribute no
/// entry.
fn command(tokens: &[String]) -> ExecCommand {
    if tokens.iter().all(String::is_empty) {
        ExecCommand::empty()
    } else {
        ExecCommand::new(tokens.to_vec())
    }
}

/// Parse a numeric directive, enforcing the contract's zero policy (plan §1.1).
fn number(line: usize, key: &str, tokens: &[String], zero: Zero) -> Result<u64, ParseError> {
    let text = join_tokens(tokens);
    let value: u64 = text
        .parse()
        .map_err(|_| invalid(line, key, &text, "expected a non-negative integer"))?;
    if zero == Zero::Rejected && value == 0 {
        return Err(invalid(
            line,
            key,
            &text,
            "0 is not allowed for this directive",
        ));
    }
    Ok(value)
}

fn boolean(line: usize, key: &str, tokens: &[String]) -> Result<bool, ParseError> {
    let text = join_tokens(tokens);
    match text.as_str() {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        _ => Err(invalid(line, key, &text, "expected 'true' or 'false'")),
    }
}

fn caps(line: usize, key: &str, tokens: &[String]) -> Result<Vec<Cap>, ParseError> {
    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens {
        let cap = Cap::parse(token)
            .ok_or_else(|| invalid(line, key, token, "not a valid capability name"))?;
        out.push(cap);
    }
    Ok(out)
}

fn environment(
    line: usize,
    key: &str,
    tokens: &[String],
) -> Result<Vec<(String, String)>, ParseError> {
    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens {
        let (k, v) = token
            .split_once('=')
            .ok_or_else(|| invalid(line, key, token, "expected KEY=VALUE"))?;
        if k.is_empty() {
            return Err(invalid(line, key, token, "the variable name is empty"));
        }
        out.push((k.to_string(), v.to_string()));
    }
    Ok(out)
}

fn size(line: usize, key: &str, text: &str) -> Result<SizeBytes, ParseError> {
    // Plan §1.1: only `RestartSec`/`StartLimitIntervalSec`/`Timeout*Sec` accept
    // 0; a zero byte limit is meaningless (`None` already means unlimited).
    match SizeBytes::parse(text) {
        Some(value) if value.0 == 0 => Err(invalid(line, key, text, "0 is not a valid limit")),
        Some(value) => Ok(value),
        None => Err(invalid(
            line,
            key,
            text,
            "expected a byte count with an optional K/M/G suffix",
        )),
    }
}

/// Plan §1.1: these two directives only accept absolute paths.
fn require_absolute(line: usize, key: &str, path: &str) -> Result<(), ParseError> {
    if path.starts_with('/') {
        Ok(())
    } else {
        Err(invalid(line, key, path, "expected an absolute path"))
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;
    use alloc::vec;

    use super::*;

    fn name(s: &str) -> UnitName {
        UnitName::parse(s).expect("valid unit name")
    }

    /// Parse a unit file, asserting success.
    fn ok(unit: &str, text: &str) -> UnitConfig {
        parse(&name(unit), text).unwrap_or_else(|e| panic!("{unit} should parse, got: {e}"))
    }

    /// Parse and expect failure, returning the error for inspection.
    fn err(unit: &str, text: &str) -> ParseError {
        parse(&name(unit), text).expect_err("expected a parse error")
    }

    // --- happy path ----------------------------------------------------

    #[test]
    fn parses_a_minimal_service() {
        let cfg = ok(
            "app.service",
            "[Unit]\nDescription=App\n\n[Service]\nExecStart=/bin/true\n",
        );
        assert_eq!(cfg.name, name("app.service"));
        assert_eq!(cfg.description, "App");
        assert_eq!(cfg.exec_start.argv(), ["/bin/true"]);
        assert_eq!(cfg.service_kind, ServiceKind::Simple, "the default");
    }

    /// A target is a process-less aggregation point and needs no command
    /// (plan §7.2).
    #[test]
    fn a_target_needs_no_exec_start() {
        let cfg = ok("net.target", "[Unit]\nDescription=Network\n");
        assert!(cfg.is_target());
        assert!(cfg.exec_start.is_empty());
    }

    #[test]
    fn a_service_without_exec_start_is_rejected() {
        assert_eq!(
            err("app.service", "[Unit]\nDescription=App\n"),
            ParseError::MissingExecStart {
                unit: "app.service".to_string()
            }
        );
    }

    #[test]
    fn an_empty_file_is_a_missing_exec_start() {
        assert!(matches!(
            err("app.service", ""),
            ParseError::MissingExecStart { .. }
        ));
    }

    // --- comments, blank lines, whitespace -----------------------------

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let cfg = ok(
            "app.service",
            "# leading\n\n[Unit]\n# about it\nDescription=App\n\n[Service]\nExecStart=/bin/true # trailing\n",
        );
        assert_eq!(cfg.description, "App");
    }

    /// A bare `#` ends the value wherever it appears, no whitespace needed.
    #[test]
    fn a_bare_hash_ends_the_value_anywhere() {
        let cfg = ok(
            "app.service",
            "[Unit]\nDescription=hello#not a comment\n[Service]\nExecStart=/bin/true\n",
        );
        assert_eq!(cfg.description, "hello");
    }

    /// Plan §1.0: a literal `#` must be escaped or quoted.
    #[test]
    fn an_escaped_hash_is_literal() {
        let cfg = ok(
            "app.service",
            "[Unit]\nDescription=a\\#b\n[Service]\nExecStart=/bin/true\n",
        );
        assert_eq!(cfg.description, "a#b");
    }

    #[test]
    fn a_quoted_hash_is_literal() {
        let cfg = ok(
            "app.service",
            "[Unit]\nDescription=\"a # b\"\n[Service]\nExecStart=/bin/true\n",
        );
        assert_eq!(cfg.description, "a # b");
    }

    /// The example from plan §1.0: `PATH=/usr/bin\#app` keeps the `#`.
    #[test]
    fn environment_keeps_an_escaped_hash() {
        let cfg = ok(
            "app.service",
            "[Service]\nExecStart=/bin/true\nEnvironment=FOO=bar PATH=/usr/bin\\#app\n",
        );
        assert_eq!(
            cfg.environment,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("PATH".to_string(), "/usr/bin#app".to_string()),
            ]
        );
    }

    /// Plan §1.0: leading/trailing whitespace is stripped, internal runs
    /// collapse to one space.
    #[test]
    fn internal_whitespace_collapses() {
        let cfg = ok(
            "app.service",
            "[Unit]\nDescription=   lots    of   space   \n[Service]\nExecStart=/bin/true\n",
        );
        assert_eq!(cfg.description, "lots of space");
    }

    // --- quoting and escapes -------------------------------------------

    /// Plan §1.0 example 1.
    #[test]
    fn a_quoted_argument_stays_one_argv_entry() {
        let cfg = ok(
            "app.service",
            "[Service]\nExecStart=/bin/echo \"hello world\"\n",
        );
        assert_eq!(cfg.exec_start.argv(), ["/bin/echo", "hello world"]);
    }

    /// Plan §1.0 example 2: an explicit shell is ordinary arguments.
    #[test]
    fn an_explicit_shell_is_ordinary_argv() {
        let cfg = ok(
            "app.service",
            "[Service]\nExecStart=/bin/sh -c \"echo hi\"\n",
        );
        assert_eq!(cfg.exec_start.argv(), ["/bin/sh", "-c", "echo hi"]);
    }

    /// Plan §1.0 example 3: `\ ` is a literal space inside a token.
    #[test]
    fn an_escaped_space_stays_in_one_argv_entry() {
        let cfg = ok("app.service", "[Service]\nExecStart=echo a\\ b\n");
        assert_eq!(cfg.exec_start.argv(), ["echo", "a b"]);
    }

    /// Plan §1.0 example 4: redirection is *not* interpreted.
    #[test]
    fn redirection_is_not_interpreted() {
        let cfg = ok("app.service", "[Service]\nExecStart=echo hi > /tmp/x\n");
        assert_eq!(cfg.exec_start.argv(), ["echo", "hi", ">", "/tmp/x"]);
    }

    /// Plan §1.0: no shell means no variable expansion.
    #[test]
    fn variables_are_not_expanded() {
        let cfg = ok("app.service", "[Service]\nExecStart=echo $HOME ${HOME}\n");
        assert_eq!(cfg.exec_start.argv(), ["echo", "$HOME", "${HOME}"]);
    }

    #[test]
    fn named_escape_sequences_work() {
        let cfg = ok(
            "app.service",
            "[Unit]\nDescription=a\\sb\\tc\\nd\\\"e\\\\f\n[Service]\nExecStart=/bin/true\n",
        );
        assert_eq!(cfg.description, "a b\tc\nd\"e\\f");
    }

    /// An unknown escape yields the character itself rather than an error.
    #[test]
    fn an_unknown_escape_is_literal() {
        let cfg = ok(
            "app.service",
            "[Unit]\nDescription=a\\qb\n[Service]\nExecStart=/bin/true\n",
        );
        assert_eq!(cfg.description, "aqb");
    }

    #[test]
    fn an_unterminated_quote_is_an_error() {
        assert_eq!(
            err("app.service", "[Service]\nExecStart=/bin/echo \"oops\n"),
            ParseError::UnterminatedQuote { line: 2 }
        );
    }

    /// Plan §1.1: an empty `ExecStart` is `MissingExecStart`, not a generic
    /// `InvalidValue` - the dedicated error is what makes a unit missing its
    /// command stand out in a boot log.
    #[test]
    fn an_empty_exec_start_is_a_missing_command() {
        assert_eq!(
            err("app.service", "[Service]\nExecStart=\n"),
            ParseError::MissingExecStart {
                unit: "app.service".to_string()
            }
        );
    }

    /// `ExecStop=` / `HealthCheck=` are `String`-typed with an empty default, so
    /// an explicit empty value is legal (plan §1.1).
    #[test]
    fn an_empty_exec_stop_and_health_check_are_legal() {
        let cfg = ok(
            "app.service",
            "[Service]\nExecStart=/bin/true\nExecStop=\nHealthCheck=\n",
        );
        assert!(cfg.exec_stop.is_empty(), "ExecStop= means SIGTERM");
        assert!(cfg.health_check.is_empty(), "HealthCheck= means no probing");
    }

    /// `ExecStartPre=`/`ExecStartPost=` are `Vec`s, so an empty value is an
    /// empty list, not an error (plan §1.1).
    #[test]
    fn an_empty_start_pre_or_post_contributes_no_entry() {
        let cfg = ok(
            "app.service",
            "[Service]\nExecStartPre=\nExecStartPost=\nExecStart=/bin/true\n",
        );
        assert!(cfg.exec_start_pre.is_empty(), "{:?}", cfg.exec_start_pre);
        assert!(cfg.exec_start_post.is_empty(), "{:?}", cfg.exec_start_post);
    }

    /// An explicitly empty quoted command is the same "unset" as a bare one.
    #[test]
    fn an_empty_quoted_command_is_unset() {
        assert_eq!(
            err("app.service", "[Service]\nExecStart=\"\"\n"),
            ParseError::MissingExecStart {
                unit: "app.service".to_string()
            }
        );
    }

    // --- continuations --------------------------------------------------

    #[test]
    fn a_backslash_continues_onto_the_next_line() {
        let cfg = ok(
            "app.service",
            "[Unit]\nDescription=first \\\n  second\n[Service]\nExecStart=/bin/true\n",
        );
        assert_eq!(cfg.description, "first second");
    }

    /// The reported line must point at the directive the operator wrote, not
    /// at the continuation line.
    #[test]
    fn a_continuation_reports_the_starting_line() {
        assert_eq!(
            err(
                "app.service",
                "[Service]\nExecStart=/bin/true \\\n   \"oops\n"
            ),
            ParseError::UnterminatedQuote { line: 2 }
        );
    }

    /// A comment line ending in `\` is a comment, not a continuation.
    #[test]
    fn a_backslash_inside_a_comment_does_not_continue() {
        let cfg = ok(
            "app.service",
            "[Unit]\n# comment \\\nDescription=App\n[Service]\nExecStart=/bin/true\n",
        );
        assert_eq!(cfg.description, "App");
    }

    // --- sections and keys ---------------------------------------------

    #[test]
    fn an_unknown_section_is_an_error() {
        assert_eq!(
            err("app.service", "[Servcie]\nExecStart=/bin/true\n"),
            ParseError::UnknownSection {
                line: 1,
                section: "Servcie".to_string()
            }
        );
    }

    /// A typo'd key would otherwise be a silent no-op, so it must be loud.
    #[test]
    fn an_unknown_key_is_an_error() {
        assert_eq!(
            err("app.service", "[Service]\nExecStrt=/bin/true\n"),
            ParseError::UnknownKey {
                line: 2,
                section: "Service",
                key: "ExecStrt".to_string()
            }
        );
    }

    /// Keys are scoped to their section: `[Unit] Requires=` is not a
    /// `[Service]` directive.
    #[test]
    fn a_unit_section_key_is_not_valid_in_service() {
        assert!(matches!(
            err("app.service", "[Service]\nRequires=other.service\n"),
            ParseError::UnknownKey {
                section: "Service",
                ..
            }
        ));
    }

    #[test]
    fn an_entry_before_any_section_is_an_error() {
        assert_eq!(
            err(
                "app.service",
                "Description=orphan\n[Service]\nExecStart=/bin/true\n"
            ),
            ParseError::EntryOutsideSection {
                line: 1,
                key: "Description".to_string()
            }
        );
    }

    #[test]
    fn a_duplicate_key_is_an_error() {
        assert_eq!(
            err(
                "app.service",
                "[Service]\nExecStart=/bin/true\nExecStart=/bin/false\n"
            ),
            ParseError::DuplicateKey {
                line: 3,
                key: "ExecStart".to_string()
            }
        );
    }

    /// `Type=` legitimately appears once per section; that must not trip the
    /// duplicate check, since the two mean different things.
    #[test]
    fn type_is_allowed_once_in_each_section() {
        let cfg = ok(
            "app.service",
            "[Unit]\nType=service\n[Service]\nType=oneshot\nExecStart=/bin/true\n",
        );
        assert_eq!(cfg.service_kind, ServiceKind::Oneshot);
    }

    #[test]
    fn a_line_without_an_equals_sign_is_a_syntax_error() {
        assert!(matches!(
            err("app.service", "[Service]\nExecStart\n"),
            ParseError::Syntax { line: 2, .. }
        ));
    }

    #[test]
    fn a_section_header_with_trailing_content_is_rejected() {
        assert!(matches!(
            err("app.service", "[Service] junk\nExecStart=/bin/true\n"),
            ParseError::Syntax { line: 1, .. }
        ));
    }

    #[test]
    fn an_unterminated_section_header_is_rejected() {
        assert!(matches!(
            err("app.service", "[Service\nExecStart=/bin/true\n"),
            ParseError::Syntax { line: 1, .. }
        ));
    }

    /// The EBNF allows CRLF.
    #[test]
    fn crlf_line_endings_are_accepted() {
        let cfg = ok(
            "app.service",
            "[Unit]\r\nDescription=App\r\n[Service]\r\nExecStart=/bin/true\r\n",
        );
        assert_eq!(cfg.description, "App");
    }

    // --- the two Type keys ---------------------------------------------

    /// Plan §1.1: a suffix/`Type=` disagreement is rejected, not overridden.
    #[test]
    fn a_unit_type_conflicting_with_the_suffix_is_rejected() {
        assert_eq!(
            err(
                "app.service",
                "[Unit]\nType=target\n[Service]\nExecStart=/bin/true\n"
            ),
            ParseError::TypeSuffixConflict {
                unit: "app.service".to_string(),
                declared: "target",
                implied: "service",
            }
        );
    }

    #[test]
    fn a_unit_type_agreeing_with_the_suffix_is_accepted() {
        assert!(ok("net.target", "[Unit]\nType=target\n").is_target());
    }

    /// Omitting `Type=` is not a conflict; the suffix decides.
    #[test]
    fn an_absent_unit_type_falls_back_to_the_suffix() {
        assert!(ok("net.target", "[Unit]\nDescription=x\n").is_target());
        assert!(
            !ok(
                "app.service",
                "[Unit]\nDescription=x\n[Service]\nExecStart=/bin/true\n"
            )
            .is_target()
        );
    }

    #[test]
    fn an_invalid_unit_type_is_rejected() {
        assert!(matches!(
            err(
                "app.service",
                "[Unit]\nType=widget\n[Service]\nExecStart=/bin/true\n"
            ),
            ParseError::InvalidValue { line: 2, .. }
        ));
    }

    /// `forking` is explicitly deferred to plan §7.10.
    #[test]
    fn the_forking_service_kind_is_not_accepted_yet() {
        assert!(matches!(
            err(
                "app.service",
                "[Service]\nType=forking\nExecStart=/bin/true\n"
            ),
            ParseError::InvalidValue { line: 2, .. }
        ));
    }

    // --- dependency directives -----------------------------------------

    /// Plan §1.1: a bare name defaults to `.service`.
    #[test]
    fn bare_dependency_names_default_to_service() {
        let cfg = ok(
            "app.service",
            "[Unit]\nRequires=database network.target\n[Service]\nExecStart=/bin/true\n",
        );
        assert_eq!(
            cfg.deps.requires,
            [name("database.service"), name("network.target")]
        );
    }

    #[test]
    fn every_dependency_directive_is_collected() {
        let cfg = ok(
            "app.service",
            "[Unit]\nRequires=a\nWants=b\nAfter=c\nBefore=d\nConflicts=e\n[Service]\nExecStart=/bin/true\n",
        );
        assert_eq!(cfg.deps.requires, [name("a.service")]);
        assert_eq!(cfg.deps.wants, [name("b.service")]);
        assert_eq!(cfg.deps.after, [name("c.service")]);
        assert_eq!(cfg.deps.before, [name("d.service")]);
        assert_eq!(cfg.deps.conflicts, [name("e.service")]);
    }

    #[test]
    fn an_empty_dependency_list_is_an_empty_list() {
        let cfg = ok(
            "app.service",
            "[Unit]\nRequires=\n[Service]\nExecStart=/bin/true\n",
        );
        assert!(cfg.deps.requires.is_empty());
    }

    /// Whitespace separates list items, so each token is validated on its own.
    /// Here the offending character is one `UnitName::parse` genuinely refuses.
    #[test]
    fn an_invalid_dependency_name_is_rejected_with_its_line() {
        assert_eq!(
            err(
                "app.service",
                "[Unit]\nRequires=good.service bad/name\n[Service]\nExecStart=/bin/true\n"
            ),
            ParseError::InvalidValue {
                line: 2,
                key: "Requires".to_string(),
                value: "bad/name".to_string(),
                reason: "invalid unit name: \"bad/name\"".to_string(),
            }
        );
    }

    /// Bare words are legal list items and each gets the `.service` suffix.
    #[test]
    fn a_multi_word_dependency_list_yields_one_item_per_word() {
        let cfg = ok(
            "app.service",
            "[Unit]\nRequires=alpha beta gamma\n[Service]\nExecStart=/bin/true\n",
        );
        assert_eq!(
            cfg.deps.requires,
            [
                name("alpha.service"),
                name("beta.service"),
                name("gamma.service")
            ]
        );
    }

    // --- value contract -------------------------------------------------

    /// Plan §1.1: zero is a meaningful value for these keys, so it must not be
    /// mistaken for "unset" or rejected.
    #[test]
    fn zero_is_accepted_where_the_contract_defines_meaning() {
        let cfg = ok(
            "app.service",
            "[Service]\nExecStart=/bin/true\nRestartSec=0\nTimeoutStartSec=0\nTimeoutStopSec=0\nStartLimitIntervalSec=0\n",
        );
        assert_eq!(cfg.restart_sec, 0, "restart immediately");
        assert_eq!(cfg.timeout_start_sec, 0, "no timeout");
        assert_eq!(cfg.timeout_stop_sec, 0, "SIGKILL at once");
        assert_eq!(cfg.start_limit_interval_sec, 0, "breaker disabled");
    }

    #[test]
    fn a_negative_number_is_rejected() {
        assert!(matches!(
            err(
                "app.service",
                "[Service]\nExecStart=/bin/true\nRestartSec=-1\n"
            ),
            ParseError::InvalidValue { line: 3, .. }
        ));
    }

    /// Plan §1.1: every numeric directive *other* than the four above rejects
    /// 0 with `InvalidValue`.
    #[test]
    fn zero_is_rejected_where_the_contract_gives_it_no_meaning() {
        let cases = [
            "StartLimitBurst=0",
            "HealthIntervalSec=0",
            "HealthTimeoutSec=0",
            "CgroupTasksMax=0",
            "CgroupMemoryMax=0",
            "CgroupMemoryHigh=0",
        ];
        for line in cases {
            let text = format!("[Service]\nExecStart=/bin/true\n{line}\n");
            assert!(
                matches!(
                    err("app.service", &text),
                    ParseError::InvalidValue { line: 3, .. }
                ),
                "{line} must reject 0"
            );
        }
    }

    /// A zero byte limit is meaningless: `None` is already "unlimited".
    #[test]
    fn a_zero_size_limit_is_rejected_with_a_specific_reason() {
        let ParseError::InvalidValue { reason, .. } = err(
            "app.service",
            "[Service]\nExecStart=/bin/true\nCgroupMemoryMax=0\n",
        ) else {
            panic!("expected InvalidValue");
        };
        assert!(reason.contains('0'), "got: {reason}");
    }

    #[test]
    fn a_non_numeric_value_is_rejected() {
        assert!(matches!(
            err(
                "app.service",
                "[Service]\nExecStart=/bin/true\nRestartSec=soon\n"
            ),
            ParseError::InvalidValue { line: 3, .. }
        ));
    }

    #[test]
    fn boolean_directives_parse() {
        let cfg = ok(
            "app.service",
            "[Service]\nExecStart=/bin/true\nNoNewPrivileges=true\nHealthRestartOnFailure=false\n",
        );
        assert!(cfg.no_new_privileges);
        assert!(!cfg.health_restart_on_failure);
    }

    /// Plan §1.0: an empty boolean is not the same as false.
    #[test]
    fn an_empty_boolean_is_rejected() {
        assert!(matches!(
            err(
                "app.service",
                "[Service]\nExecStart=/bin/true\nNoNewPrivileges=\n"
            ),
            ParseError::InvalidValue { line: 3, .. }
        ));
    }

    #[test]
    fn a_nonsense_boolean_is_rejected() {
        assert!(matches!(
            err(
                "app.service",
                "[Service]\nExecStart=/bin/true\nNoNewPrivileges=maybe\n"
            ),
            ParseError::InvalidValue { line: 3, .. }
        ));
    }

    /// Plan §1.1: `Option<Vec<Cap>>` must distinguish unset from cleared, or
    /// `CapabilityBoundingSet=` would collide with "inherit".
    #[test]
    fn capability_lists_distinguish_unset_from_cleared() {
        let unset = ok("app.service", "[Service]\nExecStart=/bin/true\n");
        assert_eq!(unset.capability_bounding_set, None, "unset means inherit");
        assert_eq!(unset.ambient_capabilities, None);

        let cleared = ok(
            "app.service",
            "[Service]\nExecStart=/bin/true\nCapabilityBoundingSet=\n",
        );
        assert_eq!(cleared.capability_bounding_set, Some(vec![]), "cleared");
    }

    #[test]
    fn a_capability_list_parses() {
        let cfg = ok(
            "app.service",
            "[Service]\nExecStart=/bin/true\nAmbientCapabilities=net_bind_service cap_kill\n",
        );
        let caps = cfg.ambient_capabilities.expect("set");
        assert_eq!(caps.len(), 2);
        assert_eq!(caps[0].as_str(), "net_bind_service");
    }

    /// Plan §1.1: relative paths are rejected to avoid CWD dependence.
    #[test]
    fn path_directives_require_an_absolute_path() {
        assert!(matches!(
            err(
                "app.service",
                "[Service]\nExecStart=/bin/true\nWorkingDirectory=var/lib\n"
            ),
            ParseError::InvalidValue { line: 3, .. }
        ));
        assert!(matches!(
            err(
                "app.service",
                "[Service]\nExecStart=/bin/true\nEnvironmentFile=etc/env\n"
            ),
            ParseError::InvalidValue { line: 3, .. }
        ));
    }

    #[test]
    fn an_absolute_path_is_accepted() {
        let cfg = ok(
            "app.service",
            "[Service]\nExecStart=/bin/true\nWorkingDirectory=/var/lib/app\nEnvironmentFile=/etc/default/app\n",
        );
        assert_eq!(cfg.working_directory, "/var/lib/app");
        assert_eq!(cfg.environment_file, "/etc/default/app");
    }

    /// An *empty* path directive is "unset", not a relative path: only a
    /// non-empty value must be absolute (plan §1.1).
    #[test]
    fn an_empty_path_directive_is_unset_not_relative() {
        let cfg = ok(
            "app.service",
            "[Service]\nExecStart=/bin/true\nWorkingDirectory=\nEnvironmentFile=\n",
        );
        assert_eq!(
            cfg.working_directory, "/",
            "an empty WorkingDirectory keeps the / default"
        );
        assert_eq!(cfg.environment_file, "");
    }

    /// Plan §1.1: the default working directory is `/`.
    #[test]
    fn working_directory_defaults_to_root() {
        let cfg = ok("app.service", "[Service]\nExecStart=/bin/true\n");
        assert_eq!(cfg.working_directory, "/");
    }

    // --- EnvironmentFile (plan §1.1) ------------------------------------

    #[test]
    fn environment_file_parses_key_value_lines() {
        let body = "FOO=bar\nPATH=/usr/bin:/bin\nEMPTY=\n";
        assert_eq!(
            parse_environment_file(body),
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ("EMPTY".to_string(), String::new()),
            ]
        );
    }

    /// A bare `#` is a comment and blank lines are ignored, as in a unit file.
    #[test]
    fn environment_file_ignores_comments_and_blanks() {
        let body = "# a comment\n\nFOO=bar # trailing comment\n   \nBAZ=qux\n";
        assert_eq!(
            parse_environment_file(body),
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string()),
            ]
        );
    }

    /// A line that is not `KEY=VALUE` is skipped, not fatal: an environment
    /// file must never take the boot down.
    #[test]
    fn environment_file_skips_malformed_lines() {
        let body = "NOEQUALS\n=novalue\nGOOD=1\n";
        assert_eq!(
            parse_environment_file(body),
            vec![("GOOD".to_string(), "1".to_string())]
        );
    }

    /// Values keep their inner spaces and are not variable-expanded.
    #[test]
    fn environment_file_values_are_literal() {
        let body = "MSG=hello world\nEXPAND=${HOME}\n";
        assert_eq!(
            parse_environment_file(body),
            vec![
                ("MSG".to_string(), "hello world".to_string()),
                ("EXPAND".to_string(), "${HOME}".to_string()),
            ]
        );
    }

    #[test]
    fn an_environment_entry_must_be_key_equals_value() {
        assert!(matches!(
            err(
                "app.service",
                "[Service]\nExecStart=/bin/true\nEnvironment=NOEQUALS\n"
            ),
            ParseError::InvalidValue { line: 3, .. }
        ));
    }

    #[test]
    fn a_quoted_environment_value_may_contain_spaces() {
        let cfg = ok(
            "app.service",
            "[Service]\nExecStart=/bin/true\nEnvironment=\"MSG=hello world\"\n",
        );
        assert_eq!(
            cfg.environment,
            vec![("MSG".to_string(), "hello world".to_string())]
        );
    }

    /// Plan §1.1 types `Environment` as `Vec<(K,V)>`, so repeating it is legal
    /// and accumulates.
    #[test]
    fn repeated_environment_lines_accumulate() {
        let cfg = ok(
            "app.service",
            "[Service]\nExecStart=/bin/true\nEnvironment=A=1\nEnvironment=B=2\n",
        );
        assert_eq!(
            cfg.environment,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "2".to_string()),
            ]
        );
    }

    /// The accumulate exception must not leak to single-valued directives.
    #[test]
    fn other_directives_still_reject_repetition() {
        assert!(matches!(
            err(
                "app.service",
                "[Unit]\nDescription=a\nDescription=b\n[Service]\nExecStart=/bin/true\n"
            ),
            ParseError::DuplicateKey { key, .. } if key == "Description"
        ));
        // Nor may it leak to single-valued [Service] directives.
        assert!(matches!(
            err("app.service", "[Service]\nExecStart=/bin/true\nExecStart=/bin/false\n"),
            ParseError::DuplicateKey { key, .. } if key == "ExecStart"
        ));
    }

    /// Splitting a long dependency list across lines must accumulate, not
    /// collide - this is how operators actually write unit files.
    #[test]
    fn repeated_dependency_lines_accumulate() {
        let cfg = ok(
            "all.target",
            "[Unit]\nType=target\nRequires=a.service\nRequires=b.service c.service\n",
        );
        assert_eq!(
            cfg.deps.requires,
            [name("a.service"), name("b.service"), name("c.service")]
        );
    }

    #[test]
    fn repeated_after_lines_accumulate() {
        let cfg = ok(
            "app.service",
            "[Unit]\nAfter=a.service\nAfter=b.service\n[Service]\nExecStart=/bin/true\n",
        );
        assert_eq!(cfg.deps.after, [name("a.service"), name("b.service")]);
    }

    /// `ExecStartPre`/`ExecStartPost` are `Vec<ExecCommand>`: each line is its
    /// own command, tokenised separately.
    #[test]
    fn repeated_exec_start_pre_lines_become_separate_commands() {
        let cfg = ok(
            "app.service",
            "[Service]\nExecStartPre=/bin/echo one\nExecStartPre=/bin/echo two\nExecStart=/bin/true\n",
        );
        assert_eq!(cfg.exec_start_pre.len(), 2);
        assert_eq!(cfg.exec_start_pre[0].argv(), ["/bin/echo", "one"]);
        assert_eq!(cfg.exec_start_pre[1].argv(), ["/bin/echo", "two"]);
    }

    #[test]
    fn cgroup_directives_parse() {
        let cfg = ok(
            "app.service",
            "[Service]\nExecStart=/bin/true\nCgroupMemoryMax=32M\nCgroupMemoryHigh=24M\nCgroupCPUMax=50%\nCgroupTasksMax=64\n",
        );
        assert_eq!(cfg.cgroup_memory_max, Some(SizeBytes(32 * 1024 * 1024)));
        assert_eq!(cfg.cgroup_memory_high, Some(SizeBytes(24 * 1024 * 1024)));
        assert_eq!(cfg.cgroup_cpu_max, Some(CpuQuota::Percent(50)));
        assert_eq!(cfg.cgroup_tasks_max, Some(64));
    }

    #[test]
    fn a_bad_cgroup_value_is_rejected() {
        assert!(matches!(
            err(
                "app.service",
                "[Service]\nExecStart=/bin/true\nCgroupCPUMax=lots\n"
            ),
            ParseError::InvalidValue { line: 3, .. }
        ));
    }

    /// The complete example from plan §1.3(1) must parse verbatim.
    #[test]
    fn the_plans_full_example_parses() {
        let cfg = ok(
            "example-daemon.service",
            r#"
# comment line
[Unit]
Description=Example daemon
Documentation=https://example.com
Requires=network.target dbus.service
Wants=metrics.socket
After=network.target
Before=app.service
Conflicts=debug.service

[Service]
Type=simple
ExecStart=/usr/bin/example-daemon --foreground
ExecStop=/usr/bin/example-daemon --stop
ExecStartPre=/usr/bin/mkdir -p /var/lib/example
ExecStartPost=/usr/bin/echo started
Restart=on-failure
RestartSec=2
StartLimitBurst=5
StartLimitIntervalSec=10
TimeoutStartSec=30
TimeoutStopSec=10
User=nobody
Group=nogroup
NoNewPrivileges=true
Environment=FOO=bar PATH=/usr/bin
EnvironmentFile=/etc/default/example
WorkingDirectory=/var/lib/example
HealthCheck=/usr/bin/example-health-check
HealthIntervalSec=5
HealthTimeoutSec=10
HealthRestartOnFailure=true
CgroupMemoryMax=32M
CgroupCPUMax=50%
CgroupTasksMax=64
"#,
        );
        assert_eq!(cfg.description, "Example daemon");
        assert_eq!(cfg.documentation, "https://example.com");
        assert_eq!(cfg.deps.requires.len(), 2);
        assert_eq!(cfg.deps.conflicts, [name("debug.service")]);
        assert_eq!(cfg.restart, RestartPolicy::OnFailure);
        assert_eq!(cfg.user, "nobody");
        assert_eq!(cfg.exec_start_pre.len(), 1);
        assert!(cfg.health_restart_on_failure);
    }

    // --- error messages -------------------------------------------------

    /// A broken unit file is the commonest operator error, so the message must
    /// locate it precisely.
    #[test]
    fn errors_name_the_line_and_key() {
        let msg = err(
            "app.service",
            "[Service]\nExecStart=/bin/true\nRestart=maybe\n",
        )
        .to_string();
        assert!(msg.contains("line 3"), "got: {msg}");
        assert!(msg.contains("Restart"), "got: {msg}");
    }
}

/// Property tests (plan §4.4: the parser is the fuzz target).
///
/// The parser is the boundary between operator-supplied text and everything
/// downstream, so the properties that matter are: it never panics, and it never
/// invents a configuration it was not given.
#[cfg(test)]
mod proptests {
    use alloc::format;
    use alloc::string::ToString;

    use proptest::prelude::*;

    use super::*;

    // Any input at all must produce a result rather than a panic or a hang.
    proptest! {
        #[test]
        fn parse_never_panics(raw in ".{0,300}") {
            let _ = parse(&UnitName::parse("app.service").unwrap(), &raw);
        }
    }

    // Unicode and control characters must not break the lexer.
    proptest! {
        #[test]
        fn parse_never_panics_on_arbitrary_unicode(raw in ".{0,120}") {
            let _ = parse(&UnitName::parse("app.service").unwrap(), &raw);
        }
    }

    // Successful parses must always yield the name they were given: the
    // parser must never rename a unit.
    proptest! {
        #[test]
        fn a_parsed_config_keeps_its_name(unit in "[a-z]{1,8}\\.service") {
            let name = UnitName::parse(&unit).unwrap();
            let text = "[Service]\nExecStart=/bin/true\n".to_string();
            if let Ok(cfg) = parse(&name, &text) {
                prop_assert_eq!(cfg.name, name);
            }
        }
    }

    // A service that parsed successfully must have a usable command; a target
    // need not. This pins the §1.1 invariant across random inputs.
    proptest! {
        #[test]
        fn exec_start_invariant_holds_for_any_input(raw in ".{0,200}") {
            let name = UnitName::parse("app.service").unwrap();
            if let Ok(cfg) = parse(&name, &raw) {
                prop_assert!(
                    cfg.is_target() || !cfg.exec_start.is_empty(),
                    "a parsed service must define ExecStart"
                );
            }
        }
    }

    // `Description` is a single-line string: a parse that succeeds can never
    // have folded a continuation into it, and it must never contain a newline
    // unless it was escaped on purpose.
    proptest! {
        #[test]
        fn description_never_swallows_the_next_directive(raw in ".{0,200}") {
            let name = UnitName::parse("app.service").unwrap();
            let text = format!("[Unit]\nDescription={raw}\n[Service]\nExecStart=/bin/true\n");
            if let Ok(cfg) = parse(&name, &text) {
                prop_assert!(
                    !cfg.description.contains("[Service]"),
                    "a value must not swallow the next section: {:?}",
                    cfg.description
                );
            }
        }
    }

    // Parsing is deterministic: the same text always yields the same config.
    // (UnitConfig's Eq ignores nothing, so this compares the whole struct.)
    proptest! {
        #[test]
        fn parsing_is_deterministic(raw in ".{0,200}") {
            let name = UnitName::parse("app.service").unwrap();
            let a = parse(&name, &raw);
            let b = parse(&name, &raw);
            prop_assert_eq!(a, b);
        }
    }

    // A quoted argument survives round-tripping through `argv`: whatever went
    // in as one token comes back as exactly one entry.
    proptest! {
        #[test]
        fn quoted_arguments_stay_single_entries(
            word in "[a-z]{1,10}",
            tail in "[a-z]{1,10}"
        ) {
            let name = UnitName::parse("app.service").unwrap();
            let text = format!(
                "[Service]\nExecStart=/bin/echo \"{word} {tail}\"\n"
            );
            let cfg = parse(&name, &text).expect("must parse");
            prop_assert_eq!(
                cfg.exec_start.argv(),
                &["/bin/echo".to_string(), format!("{word} {tail}")],
            );
        }
    }

    // An escaped space is part of its token, not a separator.
    proptest! {
        #[test]
        fn escaped_spaces_stay_inside_one_argument(
            head in "[a-z]{1,8}",
            tail in "[a-z]{1,8}"
        ) {
            let name = UnitName::parse("app.service").unwrap();
            let text = format!("[Service]\nExecStart=echo {head}\\ {tail}\n");
            let cfg = parse(&name, &text).expect("must parse");
            prop_assert_eq!(cfg.exec_start.argv(), &["echo".to_string(), format!("{head} {tail}")]);
        }
    }

    // A bare `#` truncates the value, so whatever follows is not part of it.
    // This is the plan §1.0 rule that makes escaping necessary.
    proptest! {
        #[test]
        fn a_bare_hash_ends_the_value(
            head in "[a-z]{1,8}",
            tail in "[a-z]{1,8}"
        ) {
            let name = UnitName::parse("app.service").unwrap();
            let text = format!("[Unit]\nDescription={head}#{tail}\n[Service]\nExecStart=/bin/true\n");
            let cfg = parse(&name, &text).expect("must parse");
            prop_assert_eq!(cfg.description, head);
        }
    }

    // An escaped `#` is the documented way to keep one, and it must survive.
    proptest! {
        #[test]
        fn an_escaped_hash_survives(
            head in "[a-z]{1,8}",
            tail in "[a-z]{1,8}"
        ) {
            let name = UnitName::parse("app.service").unwrap();
            let text = format!("[Unit]\nDescription={head}\\#{tail}\n[Service]\nExecStart=/bin/true\n");
            let cfg = parse(&name, &text).expect("must parse");
            prop_assert_eq!(cfg.description, format!("{head}#{tail}"));
        }
    }

    // Dependency lists always yield names that re-parse to themselves: the
    // `Requires=` reference a unit sees later is already canonical.
    proptest! {
        #[test]
        fn dependency_names_are_canonical(
            word in "[a-z0-9]{1,10}",
            suffix in prop_oneof![Just(""), Just(".service"), Just(".target")]
        ) {
            let name = UnitName::parse("app.service").unwrap();
            let text = format!("[Unit]\nRequires={word}{suffix}\n[Service]\nExecStart=/bin/true\n");
            let cfg = parse(&name, &text).expect("must parse");
            for dep in &cfg.deps.requires {
                prop_assert_eq!(&UnitName::parse(dep.canonical()).unwrap(), dep);
            }
        }
    }
}
