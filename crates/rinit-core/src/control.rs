//! Control protocol between `rctl` and rinit (plan §1.3(10), §3 M3).
//!
//! The *framing* is binary, as plan §1.3(10) requires: every message is a
//! little-endian `u32` byte length followed by that many bytes, and every
//! request body opens with a version and a command code. The *payloads* are
//! UTF-8 text, because the status and log replies exist to be printed - a
//! structured encoding would cost code and buy nothing on a 128 MB device.
//!
//! Everything here is pure, so the decoder can be property-tested (and, per
//! plan §4.4, fuzzed) without a socket, a peer, or root. Nothing in this module
//! allocates unboundedly: a frame longer than [`MAX_FRAME_BYTES`] is rejected
//! before its body is looked at, which is what stops a hostile length prefix
//! from becoming a memory-exhaustion bug (plan §1.3(10)).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::unit::UnitName;

/// Wire version. Bump on any incompatible change; `rctl` and rinit refuse to
/// talk across a mismatch rather than degrade (plan §1.3(10)).
pub const PROTOCOL_VERSION: u16 = 1;

/// Largest frame body accepted, in bytes.
///
/// A control reply is a few lines of text; 64 KiB is generous while still
/// bounding what a client can make rinit buffer.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// Bytes of the little-endian length prefix that frames every message.
pub const LENGTH_PREFIX_BYTES: usize = 4;

/// Largest unit name accepted in a request, matching the name limit.
const MAX_UNIT_NAME_BYTES: usize = 255;

/// A request from `rctl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// List every known unit.
    List,
    /// Show one unit, or a one-line summary of all of them.
    Status {
        /// The unit to describe, or `None` for the summary.
        unit: Option<UnitName>,
        /// Lines of buffered log to include.
        tail: u32,
    },
    /// Start a unit.
    Start(UnitName),
    /// Stop a unit.
    Stop(UnitName),
    /// Restart a unit.
    Restart(UnitName),
    /// Clear a unit's failed / start-limit state without starting it.
    ResetFailed(UnitName),
    /// Read buffered log.
    Log {
        /// The unit to read, or `None` for the merged buffer.
        unit: Option<UnitName>,
        /// Lines to return.
        tail: u32,
    },
}

impl Request {
    /// The command byte this request encodes.
    pub const fn command(&self) -> Command {
        match self {
            Request::List => Command::List,
            Request::Status { .. } => Command::Status,
            Request::Start(_) => Command::Start,
            Request::Stop(_) => Command::Stop,
            Request::Restart(_) => Command::Restart,
            Request::ResetFailed(_) => Command::ResetFailed,
            Request::Log { .. } => Command::Log,
        }
    }

    /// A one-line description for rinit's log.
    pub fn describe(&self) -> String {
        match self {
            Request::List => "list".to_string(),
            Request::Status { unit, .. } => match unit {
                Some(unit) => format!("status {unit}"),
                None => "status".to_string(),
            },
            Request::Start(unit) => format!("start {unit}"),
            Request::Stop(unit) => format!("stop {unit}"),
            Request::Restart(unit) => format!("restart {unit}"),
            Request::ResetFailed(unit) => format!("reset-failed {unit}"),
            Request::Log { unit, .. } => match unit {
                Some(unit) => format!("log {unit}"),
                None => "log".to_string(),
            },
        }
    }
}

/// The command codes on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Command {
    /// `rctl list`.
    List = 1,
    /// `rctl status`.
    Status = 2,
    /// `rctl start`.
    Start = 3,
    /// `rctl stop`.
    Stop = 4,
    /// `rctl restart`.
    Restart = 5,
    /// `rctl reset-failed`.
    ResetFailed = 6,
    /// `rctl log`.
    Log = 7,
}

impl Command {
    /// The wire code.
    pub const fn byte(self) -> u8 {
        self as u8
    }

    /// Decode a wire code.
    pub const fn from_byte(byte: u8) -> Option<Command> {
        match byte {
            1 => Some(Command::List),
            2 => Some(Command::Status),
            3 => Some(Command::Start),
            4 => Some(Command::Stop),
            5 => Some(Command::Restart),
            6 => Some(Command::ResetFailed),
            7 => Some(Command::Log),
            _ => None,
        }
    }
}

/// The outcome of a request, and the source of `rctl`'s exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    /// The request succeeded.
    Ok = 0,
    /// A general failure; the payload says why.
    Error = 1,
    /// The peer speaks a different protocol version.
    VersionMismatch = 2,
    /// The command byte is not one we know.
    UnknownCommand = 3,
    /// The named unit does not exist.
    NoSuchUnit = 4,
    /// The request contradicts the unit's state (plan §3 M3's exit code 4).
    Conflict = 5,
    /// The request frame was malformed.
    BadRequest = 6,
}

impl Status {
    /// The wire code.
    pub const fn byte(self) -> u8 {
        self as u8
    }

    /// Decode a wire code.
    pub const fn from_byte(byte: u8) -> Option<Status> {
        match byte {
            0 => Some(Status::Ok),
            1 => Some(Status::Error),
            2 => Some(Status::VersionMismatch),
            3 => Some(Status::UnknownCommand),
            4 => Some(Status::NoSuchUnit),
            5 => Some(Status::Conflict),
            6 => Some(Status::BadRequest),
            _ => None,
        }
    }

    /// The `rctl` exit code this status maps to (plan §3 M3).
    ///
    /// The plan fixes the codes at 0-4, so everything that is not "unit
    /// missing" or "operation conflict" is a general error - including a
    /// version mismatch, whose message tells the user to upgrade.
    pub const fn exit_code(self) -> u8 {
        match self {
            Status::Ok => 0,
            Status::NoSuchUnit => 3,
            Status::Conflict => 4,
            Status::Error
            | Status::VersionMismatch
            | Status::UnknownCommand
            | Status::BadRequest => 1,
        }
    }
}

/// A malformed frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// The frame is shorter than the message it claims to be.
    TooShort,
    /// The declared body length exceeds [`MAX_FRAME_BYTES`].
    TooLong {
        /// The declared length.
        declared: usize,
    },
    /// The peer speaks a different protocol version.
    VersionMismatch {
        /// The version the peer sent.
        found: u16,
        /// The version this build speaks.
        expected: u16,
    },
    /// The command / status byte is not one we know.
    UnknownCode {
        /// The byte that was rejected.
        byte: u8,
    },
    /// A unit-name field was empty, too long, or not a valid name.
    BadUnitName,
    /// A text field was not valid UTF-8.
    BadText,
    /// Bytes remained after a complete message.
    TrailingBytes,
}

impl core::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ProtocolError::TooShort => write!(f, "frame is too short"),
            ProtocolError::TooLong { declared } => {
                write!(f, "frame of {declared} bytes exceeds the limit")
            }
            ProtocolError::VersionMismatch { found, expected } => write!(
                f,
                "protocol version {found} does not match {expected}; rebuild rctl"
            ),
            ProtocolError::UnknownCode { byte } => write!(f, "unknown code {byte}"),
            ProtocolError::BadUnitName => write!(f, "unit name is invalid"),
            ProtocolError::BadText => write!(f, "text field is not valid UTF-8"),
            ProtocolError::TrailingBytes => write!(f, "trailing bytes after the message"),
        }
    }
}

// --- framing -----------------------------------------------------------

/// Wrap `body` in the length prefix that frames every message.
pub fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(LENGTH_PREFIX_BYTES + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// Decode the length prefix into a body length, rejecting an over-long frame
/// before its body is read (plan §1.3(10)).
pub fn frame_len(prefix: [u8; LENGTH_PREFIX_BYTES]) -> Result<usize, ProtocolError> {
    let declared = u32::from_le_bytes(prefix) as usize;
    if declared > MAX_FRAME_BYTES {
        return Err(ProtocolError::TooLong { declared });
    }
    Ok(declared)
}

// --- requests ----------------------------------------------------------

/// Encode a request body (without the length prefix).
pub fn encode_request(request: &Request) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    out.push(request.command().byte());
    match request {
        Request::List => {}
        Request::Status { unit, tail } => {
            put_opt_unit(&mut out, unit.as_ref());
            out.extend_from_slice(&tail.to_le_bytes());
        }
        Request::Start(unit)
        | Request::Stop(unit)
        | Request::Restart(unit)
        | Request::ResetFailed(unit) => put_unit(&mut out, unit),
        Request::Log { unit, tail } => {
            put_opt_unit(&mut out, unit.as_ref());
            out.extend_from_slice(&tail.to_le_bytes());
        }
    }
    out
}

/// Decode a request body (without the length prefix).
///
/// # Errors
/// [`ProtocolError`] for a truncated frame, an unknown version or command, a
/// bad unit name, or trailing bytes.
pub fn decode_request(body: &[u8]) -> Result<Request, ProtocolError> {
    let mut decoder = Decoder::new(body);
    let version = decoder.u16()?;
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::VersionMismatch {
            found: version,
            expected: PROTOCOL_VERSION,
        });
    }
    let command =
        Command::from_byte(decoder.u8()?).ok_or(ProtocolError::UnknownCode { byte: body[2] })?;

    let request = match command {
        Command::List => Request::List,
        Command::Status => {
            let unit = decoder.opt_unit()?;
            let tail = decoder.u32()?;
            Request::Status { unit, tail }
        }
        Command::Start => Request::Start(decoder.unit()?),
        Command::Stop => Request::Stop(decoder.unit()?),
        Command::Restart => Request::Restart(decoder.unit()?),
        Command::ResetFailed => Request::ResetFailed(decoder.unit()?),
        Command::Log => {
            let unit = decoder.opt_unit()?;
            let tail = decoder.u32()?;
            Request::Log { unit, tail }
        }
    };

    if !decoder.is_empty() {
        return Err(ProtocolError::TrailingBytes);
    }
    Ok(request)
}

// --- responses ---------------------------------------------------------

/// Encode a response body (without the length prefix).
pub fn encode_response(status: Status, text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + text.len());
    out.push(status.byte());
    out.extend_from_slice(text.as_bytes());
    out
}

/// Decode a response body (without the length prefix).
///
/// # Errors
/// [`ProtocolError`] for an empty frame, an unknown status, or a non-UTF-8
/// payload.
pub fn decode_response(body: &[u8]) -> Result<(Status, String), ProtocolError> {
    let Some((&byte, text)) = body.split_first() else {
        return Err(ProtocolError::TooShort);
    };
    let status = Status::from_byte(byte).ok_or(ProtocolError::UnknownCode { byte })?;
    let text = core::str::from_utf8(text).map_err(|_| ProtocolError::BadText)?;
    Ok((status, text.to_string()))
}

// --- field helpers -----------------------------------------------------

fn put_unit(out: &mut Vec<u8>, unit: &UnitName) {
    let name = unit.canonical();
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
}

fn put_opt_unit(out: &mut Vec<u8>, unit: Option<&UnitName>) {
    match unit {
        Some(unit) => {
            out.push(1);
            put_unit(out, unit);
        }
        None => out.push(0),
    }
}

/// A bounds-checked reader; every accessor returns `TooShort` rather than
/// panicking, which is what makes the decoder safe to fuzz (plan §4.4).
struct Decoder<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Decoder { bytes, pos: 0 }
    }

    fn is_empty(&self) -> bool {
        self.pos == self.bytes.len()
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], ProtocolError> {
        let end = self.pos.checked_add(count).ok_or(ProtocolError::TooShort)?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(ProtocolError::TooShort)?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ProtocolError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, ProtocolError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn unit(&mut self) -> Result<UnitName, ProtocolError> {
        let len = self.u8()? as usize;
        if len == 0 || len > MAX_UNIT_NAME_BYTES {
            return Err(ProtocolError::BadUnitName);
        }
        let bytes = self.take(len)?;
        let text = core::str::from_utf8(bytes).map_err(|_| ProtocolError::BadText)?;
        UnitName::parse(text).map_err(|_| ProtocolError::BadUnitName)
    }

    fn opt_unit(&mut self) -> Result<Option<UnitName>, ProtocolError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.unit()?)),
            _ => Err(ProtocolError::BadUnitName),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> UnitName {
        UnitName::parse(s).expect("valid name")
    }

    fn round_trip(request: Request) {
        let body = encode_request(&request);
        assert_eq!(decode_request(&body), Ok(request.clone()), "body: {body:?}");
    }

    #[test]
    fn every_request_round_trips() {
        round_trip(Request::List);
        round_trip(Request::Status {
            unit: None,
            tail: 0,
        });
        round_trip(Request::Status {
            unit: Some(name("app.service")),
            tail: 42,
        });
        round_trip(Request::Start(name("app.service")));
        round_trip(Request::Stop(name("db.target")));
        round_trip(Request::Restart(name("net.service")));
        round_trip(Request::ResetFailed(name("app.service")));
        round_trip(Request::Log {
            unit: None,
            tail: 10,
        });
        round_trip(Request::Log {
            unit: Some(name("app.service")),
            tail: u32::MAX,
        });
    }

    #[test]
    fn a_response_round_trips() {
        let body = encode_response(Status::Ok, "hello\nworld");
        assert_eq!(
            decode_response(&body),
            Ok((Status::Ok, "hello\nworld".to_string()))
        );

        let body = encode_response(Status::NoSuchUnit, "no such unit: ghost.service");
        let (status, text) = decode_response(&body).expect("decode");
        assert_eq!(status, Status::NoSuchUnit);
        assert!(text.contains("ghost.service"));
    }

    /// A version bump must be refused, not ignored: this is the "hard reject"
    /// of plan §1.3(10).
    #[test]
    fn a_foreign_version_is_refused() {
        let mut body = encode_request(&Request::List);
        let foreign = PROTOCOL_VERSION.wrapping_add(1);
        body[..2].copy_from_slice(&foreign.to_le_bytes());
        assert_eq!(
            decode_request(&body),
            Err(ProtocolError::VersionMismatch {
                found: foreign,
                expected: PROTOCOL_VERSION
            })
        );
    }

    #[test]
    fn an_unknown_command_is_refused() {
        let mut body = encode_request(&Request::List);
        body[2] = 99;
        assert_eq!(
            decode_request(&body),
            Err(ProtocolError::UnknownCode { byte: 99 })
        );
    }

    /// Trailing bytes are a bug or an attempt to smuggle data, never accepted.
    #[test]
    fn trailing_bytes_are_refused() {
        let mut body = encode_request(&Request::List);
        body.push(0);
        assert_eq!(decode_request(&body), Err(ProtocolError::TrailingBytes));
    }

    #[test]
    fn a_truncated_request_is_refused() {
        let body = encode_request(&Request::Start(name("app.service")));
        for cut in 0..body.len() {
            assert!(
                decode_request(&body[..cut]).is_err(),
                "a {cut}-byte prefix must not decode"
            );
        }
    }

    #[test]
    fn a_bad_unit_name_is_refused() {
        // `Start` with a length byte that runs past the body.
        let body = [
            PROTOCOL_VERSION.to_le_bytes()[0],
            PROTOCOL_VERSION.to_le_bytes()[1],
            3,
            8,
            b'x',
        ];
        assert_eq!(decode_request(&body), Err(ProtocolError::TooShort));

        // A length of zero is not a unit.
        let body = [
            PROTOCOL_VERSION.to_le_bytes()[0],
            PROTOCOL_VERSION.to_le_bytes()[1],
            3,
            0,
        ];
        assert_eq!(decode_request(&body), Err(ProtocolError::BadUnitName));

        // A name `UnitName::parse` rejects.
        let bad = b"bad/name";
        let mut body = alloc::vec![
            PROTOCOL_VERSION.to_le_bytes()[0],
            PROTOCOL_VERSION.to_le_bytes()[1],
            3,
            bad.len() as u8
        ];
        body.extend_from_slice(bad);
        assert_eq!(decode_request(&body), Err(ProtocolError::BadUnitName));
    }

    #[test]
    fn an_empty_response_is_refused() {
        assert_eq!(decode_response(&[]), Err(ProtocolError::TooShort));
        assert_eq!(
            decode_response(&[200]),
            Err(ProtocolError::UnknownCode { byte: 200 })
        );
        assert_eq!(decode_response(&[0, 0xff]), Err(ProtocolError::BadText));
    }

    /// The framing rejects an over-long body on the strength of the prefix
    /// alone (plan §1.3(10)'s "拒绝超长 payload").
    #[test]
    fn an_over_long_frame_is_refused_by_its_prefix() {
        assert_eq!(frame_len(4u32.to_le_bytes()), Ok(4));
        assert_eq!(
            frame_len((MAX_FRAME_BYTES as u32 + 1).to_le_bytes()),
            Err(ProtocolError::TooLong {
                declared: MAX_FRAME_BYTES + 1
            })
        );
        assert_eq!(
            frame_len(u32::MAX.to_le_bytes()),
            Err(ProtocolError::TooLong {
                declared: u32::MAX as usize
            })
        );
    }

    #[test]
    fn framing_round_trips() {
        let body = encode_request(&Request::List);
        let wire = frame(&body);
        let (prefix, rest) = wire.split_at(LENGTH_PREFIX_BYTES);
        let len = frame_len(prefix.try_into().unwrap()).expect("length");
        assert_eq!(len, body.len());
        assert_eq!(rest, body.as_slice());
    }

    /// The exit-code contract of plan §3 M3.
    #[test]
    fn status_exit_codes_match_the_contract() {
        assert_eq!(Status::Ok.exit_code(), 0);
        assert_eq!(Status::Error.exit_code(), 1);
        assert_eq!(Status::VersionMismatch.exit_code(), 1);
        assert_eq!(Status::NoSuchUnit.exit_code(), 3);
        assert_eq!(Status::Conflict.exit_code(), 4);
    }

    #[test]
    fn describe_names_the_operation() {
        assert_eq!(
            Request::Start(name("a.service")).describe(),
            "start a.service"
        );
        assert_eq!(Request::List.describe(), "list");
        assert_eq!(
            Request::Status {
                unit: None,
                tail: 0
            }
            .describe(),
            "status"
        );
    }
}

#[cfg(test)]
mod proptests {
    use proptest::prelude::*;

    use super::*;

    /// A unit name that `UnitName::parse` accepts.
    fn unit_name() -> impl Strategy<Value = UnitName> {
        "[a-z][a-z0-9-]{0,11}(\\.service|\\.target)?".prop_map(|raw| {
            // The generated string is alphanumeric and short, so it parses.
            UnitName::parse(&raw).expect("generated name is valid")
        })
    }

    fn any_request() -> impl Strategy<Value = Request> {
        prop_oneof![
            Just(Request::List),
            (proptest::option::of(unit_name()), any::<u32>())
                .prop_map(|(unit, tail)| Request::Status { unit, tail }),
            unit_name().prop_map(Request::Start),
            unit_name().prop_map(Request::Stop),
            unit_name().prop_map(Request::Restart),
            unit_name().prop_map(Request::ResetFailed),
            (proptest::option::of(unit_name()), any::<u32>())
                .prop_map(|(unit, tail)| Request::Log { unit, tail }),
        ]
    }

    proptest! {
        /// The decoder must never panic, whatever arrives on the socket - this
        /// is the property plan §4.4 wants fuzzed.
        #[test]
        fn decoding_arbitrary_bytes_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
            let _ = decode_request(&bytes);
            let _ = decode_response(&bytes);
            let _ = frame_len(bytes.iter().copied().take(4).collect::<Vec<_>>().try_into().unwrap_or([0; 4]));
        }

        /// Encoding then decoding is the identity.
        #[test]
        fn requests_round_trip(request in any_request()) {
            prop_assert_eq!(decode_request(&encode_request(&request)), Ok(request));
        }

        /// Responses round-trip too, including a non-ASCII payload.
        #[test]
        fn responses_round_trip(text in ".*", status in 0u8..7) {
            let status = Status::from_byte(status).expect("generated in range");
            let body = encode_response(status, &text);
            prop_assert_eq!(decode_response(&body), Ok((status, text)));
        }

        /// Framing is invertible and the decoded length always matches.
        #[test]
        fn framing_round_trips(request in any_request()) {
            let body = encode_request(&request);
            let wire = frame(&body);
            let prefix: [u8; LENGTH_PREFIX_BYTES] = wire[..LENGTH_PREFIX_BYTES].try_into().unwrap();
            prop_assert_eq!(frame_len(prefix), Ok(body.len()));
            prop_assert_eq!(&wire[LENGTH_PREFIX_BYTES..], body.as_slice());
        }
    }
}
