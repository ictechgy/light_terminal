//! Trusted speculation runner protocol and Linux-only hidden runner.
//!
//! The protocol is private, generation-bound, packet-bounded, and never
//! renders attacker-controlled bytes in errors.  Candidate PTY bytes are not
//! control input and are only counted by the Linux runner.

use serde::{Deserialize, Serialize};
use std::fmt;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use uuid::Uuid;

pub(crate) const MAX_CONTROL_FRAME_BYTES: usize = 8 * 1024;
pub(crate) const MAX_ARGV_BYTES: usize = 256 * 1024;
pub(crate) const MAX_ARG_BYTES: usize = 64 * 1024;
pub(crate) const MAX_ARGV_ENTRIES: usize = 256;
pub(crate) const MAX_ARGV_CHUNK_BYTES: usize = 4 * 1024;
const CONTROL_PROTOCOL: &str = "lterm-speculation-control-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunnerProtocolError {
    InvalidIdentity,
    StaleGeneration,
    InvalidSequence,
    InvalidFrame,
    OversizedFrame,
    InvalidArgv,
    DescriptorViolation,
    Unsupported,
    Io,
}

impl RunnerProtocolError {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::InvalidIdentity => "speculation_control_invalid_identity",
            Self::StaleGeneration => "speculation_control_stale_generation",
            Self::InvalidSequence => "speculation_control_invalid_sequence",
            Self::InvalidFrame => "speculation_control_invalid_frame",
            Self::OversizedFrame => "speculation_control_oversized_frame",
            Self::InvalidArgv => "speculation_control_invalid_argv",
            Self::DescriptorViolation => "speculation_control_descriptor_violation",
            Self::Unsupported => "speculation_control_unsupported",
            Self::Io => "speculation_control_io",
        }
    }
}

impl fmt::Display for RunnerProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for RunnerProtocolError {}

pub(crate) type RunnerResult<T> = Result<T, RunnerProtocolError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunnerIdentity {
    pub tournament_uuid: Uuid,
    pub candidate_index: u8,
    pub generation: u64,
}

impl RunnerIdentity {
    pub(crate) fn validate(self) -> RunnerResult<()> {
        if self.tournament_uuid.is_nil() || self.candidate_index >= 2 || self.generation == 0 {
            return Err(RunnerProtocolError::InvalidIdentity);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DecisionKind {
    Select,
    Abort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunnerExitCategory {
    ExitedZero,
    ExitedNonzero,
    Signaled,
    SpawnFailed,
    OutputLimitExceeded,
    EvidenceIncomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlacementDescriptorEvidence {
    pub dev: u64,
    pub ino: u64,
    pub statx_mnt_id_unique: u64,
    pub candidate_index: u8,
    pub generation: u64,
}

impl PlacementDescriptorEvidence {
    pub(crate) fn validate(self, identity: RunnerIdentity) -> RunnerResult<()> {
        if self.dev == 0
            || self.ino == 0
            || self.statx_mnt_id_unique == 0
            || self.candidate_index != identity.candidate_index
            || self.generation != identity.generation
        {
            return Err(RunnerProtocolError::DescriptorViolation);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ControlMessage {
    Hello,
    HelloAck,
    ArgvBegin {
        argument_count: u16,
        total_bytes: u32,
    },
    ArgvChunk {
        argument_index: u16,
        offset: u32,
        total: u32,
        data: String,
    },
    ArgvEnd,
    Ready,
    ReadyAck {
        placement: PlacementDescriptorEvidence,
    },
    PayloadFdAck,
    Go,
    GoReceived {
        elapsed_ns: u64,
    },
    PayloadPlaced,
    PayloadRelease,
    LeaderExited {
        category: RunnerExitCategory,
    },
    OutputDrained {
        bytes: u64,
    },
    ResultAccepted,
    Decision {
        decision: DecisionKind,
    },
    Ack {
        decision: DecisionKind,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlFrame {
    protocol: String,
    pub identity: RunnerIdentity,
    pub sequence: u32,
    pub message: ControlMessage,
}

impl ControlFrame {
    pub(crate) fn new(identity: RunnerIdentity, sequence: u32, message: ControlMessage) -> Self {
        Self {
            protocol: CONTROL_PROTOCOL.into(),
            identity,
            sequence,
            message,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SequenceState {
    Hello,
    HelloAck,
    ArgvBegin,
    ArgvChunkOrEnd,
    Ready,
    ReadyAck,
    PayloadFdAck,
    Go,
    GoReceived,
    PayloadPlaced,
    PayloadRelease,
    LeaderExited,
    OutputDrained,
    ResultAccepted,
    Decision,
    Ack,
    Complete,
}

#[derive(Debug, Clone)]
pub(crate) struct SequenceValidator {
    identity: RunnerIdentity,
    next_sequence: u32,
    state: SequenceState,
}

impl SequenceValidator {
    pub(crate) fn new(identity: RunnerIdentity) -> RunnerResult<Self> {
        identity.validate()?;
        Ok(Self {
            identity,
            next_sequence: 0,
            state: SequenceState::Hello,
        })
    }

    pub(crate) fn accept(&mut self, frame: &ControlFrame) -> RunnerResult<()> {
        if frame.protocol != CONTROL_PROTOCOL
            || frame.identity.tournament_uuid != self.identity.tournament_uuid
        {
            return Err(RunnerProtocolError::InvalidIdentity);
        }
        if frame.identity.candidate_index != self.identity.candidate_index
            || frame.identity.generation != self.identity.generation
        {
            return Err(RunnerProtocolError::StaleGeneration);
        }
        if frame.sequence != self.next_sequence {
            return Err(RunnerProtocolError::InvalidSequence);
        }
        let next = match (self.state, &frame.message) {
            (SequenceState::Hello, ControlMessage::Hello) => SequenceState::HelloAck,
            (SequenceState::HelloAck, ControlMessage::HelloAck) => SequenceState::ArgvBegin,
            (SequenceState::ArgvBegin, ControlMessage::ArgvBegin { .. }) => {
                SequenceState::ArgvChunkOrEnd
            }
            (SequenceState::ArgvChunkOrEnd, ControlMessage::ArgvChunk { .. }) => {
                SequenceState::ArgvChunkOrEnd
            }
            (SequenceState::ArgvChunkOrEnd, ControlMessage::ArgvEnd) => SequenceState::Ready,
            (SequenceState::Ready, ControlMessage::Ready) => SequenceState::ReadyAck,
            (SequenceState::ReadyAck, ControlMessage::ReadyAck { placement }) => {
                placement.validate(self.identity)?;
                SequenceState::PayloadFdAck
            }
            (SequenceState::PayloadFdAck, ControlMessage::PayloadFdAck) => SequenceState::Go,
            (SequenceState::Go, ControlMessage::Go) => SequenceState::GoReceived,
            (SequenceState::GoReceived, ControlMessage::GoReceived { .. }) => {
                SequenceState::PayloadPlaced
            }
            (SequenceState::PayloadPlaced, ControlMessage::PayloadPlaced) => {
                SequenceState::PayloadRelease
            }
            (SequenceState::PayloadRelease, ControlMessage::PayloadRelease) => {
                SequenceState::LeaderExited
            }
            (SequenceState::LeaderExited, ControlMessage::LeaderExited { .. }) => {
                SequenceState::OutputDrained
            }
            (SequenceState::OutputDrained, ControlMessage::OutputDrained { .. }) => {
                SequenceState::ResultAccepted
            }
            (SequenceState::ResultAccepted, ControlMessage::ResultAccepted) => {
                SequenceState::Decision
            }
            (SequenceState::Decision, ControlMessage::Decision { .. }) => SequenceState::Ack,
            (SequenceState::Ack, ControlMessage::Ack { .. }) => SequenceState::Complete,
            _ => return Err(RunnerProtocolError::InvalidSequence),
        };
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(RunnerProtocolError::InvalidSequence)?;
        self.state = next;
        Ok(())
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.state == SequenceState::Complete
    }

    pub(crate) fn next_sequence(&self) -> u32 {
        self.next_sequence
    }
}

pub(crate) fn encode_frame(frame: &ControlFrame) -> RunnerResult<Vec<u8>> {
    let bytes = serde_json::to_vec(frame).map_err(|_| RunnerProtocolError::InvalidFrame)?;
    if bytes.is_empty() || bytes.len() > MAX_CONTROL_FRAME_BYTES {
        return Err(RunnerProtocolError::OversizedFrame);
    }
    Ok(bytes)
}

pub(crate) fn decode_frame(bytes: &[u8]) -> RunnerResult<ControlFrame> {
    if bytes.is_empty() || bytes.len() > MAX_CONTROL_FRAME_BYTES {
        return Err(RunnerProtocolError::OversizedFrame);
    }
    let frame = serde_json::from_slice::<ControlFrame>(bytes)
        .map_err(|_| RunnerProtocolError::InvalidFrame)?;
    frame.identity.validate()?;
    if frame.protocol != CONTROL_PROTOCOL {
        return Err(RunnerProtocolError::InvalidFrame);
    }
    Ok(frame)
}

#[derive(Debug)]
pub(crate) struct ArgvAssembler {
    expected_count: usize,
    expected_total: usize,
    arguments: Vec<Vec<u8>>,
    current_index: usize,
    received_total: usize,
}

impl ArgvAssembler {
    pub(crate) fn begin(argument_count: u16, total_bytes: u32) -> RunnerResult<Self> {
        let expected_count = usize::from(argument_count);
        let expected_total =
            usize::try_from(total_bytes).map_err(|_| RunnerProtocolError::InvalidArgv)?;
        if expected_count == 0
            || expected_count > MAX_ARGV_ENTRIES
            || expected_total > MAX_ARGV_BYTES
        {
            return Err(RunnerProtocolError::InvalidArgv);
        }
        Ok(Self {
            expected_count,
            expected_total,
            arguments: vec![Vec::new(); expected_count],
            current_index: 0,
            received_total: 0,
        })
    }

    pub(crate) fn push_chunk(
        &mut self,
        argument_index: u16,
        offset: u32,
        total: u32,
        data: &str,
    ) -> RunnerResult<()> {
        let argument_index = usize::from(argument_index);
        let offset = usize::try_from(offset).map_err(|_| RunnerProtocolError::InvalidArgv)?;
        let total = usize::try_from(total).map_err(|_| RunnerProtocolError::InvalidArgv)?;
        if argument_index != self.current_index
            || argument_index >= self.expected_count
            || total > MAX_ARG_BYTES
            || offset != self.arguments[argument_index].len()
        {
            return Err(RunnerProtocolError::InvalidArgv);
        }
        let decoded = decode_canonical_base64(data)?;
        if decoded.is_empty()
            || decoded.len() > MAX_ARGV_CHUNK_BYTES
            || decoded.contains(&0)
            || offset.checked_add(decoded.len()) > Some(total)
            || self.received_total.checked_add(decoded.len()) > Some(self.expected_total)
        {
            return Err(RunnerProtocolError::InvalidArgv);
        }
        self.arguments[argument_index].extend_from_slice(&decoded);
        self.received_total += decoded.len();
        if self.arguments[argument_index].len() == total {
            self.current_index += 1;
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> RunnerResult<Vec<Vec<u8>>> {
        if self.current_index != self.expected_count
            || self.received_total != self.expected_total
            || self.arguments.iter().any(Vec::is_empty)
        {
            return Err(RunnerProtocolError::InvalidArgv);
        }
        Ok(self.arguments)
    }
}

pub(crate) fn encode_base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(TABLE[(first >> 2) as usize] as char);
        encoded.push(TABLE[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            TABLE[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            TABLE[(third & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    encoded
}

pub(crate) fn argv_frames(
    identity: RunnerIdentity,
    starting_sequence: u32,
    arguments: &[Vec<u8>],
) -> RunnerResult<Vec<ControlFrame>> {
    if arguments.is_empty() || arguments.len() > MAX_ARGV_ENTRIES {
        return Err(RunnerProtocolError::InvalidArgv);
    }
    let total = arguments.iter().try_fold(0_usize, |total, argument| {
        if argument.is_empty() || argument.len() > MAX_ARG_BYTES || argument.contains(&0) {
            return Err(RunnerProtocolError::InvalidArgv);
        }
        total
            .checked_add(argument.len())
            .filter(|value| *value <= MAX_ARGV_BYTES)
            .ok_or(RunnerProtocolError::InvalidArgv)
    })?;
    let mut sequence = starting_sequence;
    let mut frames = vec![ControlFrame::new(
        identity,
        sequence,
        ControlMessage::ArgvBegin {
            argument_count: arguments
                .len()
                .try_into()
                .map_err(|_| RunnerProtocolError::InvalidArgv)?,
            total_bytes: total
                .try_into()
                .map_err(|_| RunnerProtocolError::InvalidArgv)?,
        },
    )];
    sequence = sequence
        .checked_add(1)
        .ok_or(RunnerProtocolError::InvalidSequence)?;
    for (index, argument) in arguments.iter().enumerate() {
        for (chunk_index, chunk) in argument.chunks(MAX_ARGV_CHUNK_BYTES).enumerate() {
            frames.push(ControlFrame::new(
                identity,
                sequence,
                ControlMessage::ArgvChunk {
                    argument_index: index
                        .try_into()
                        .map_err(|_| RunnerProtocolError::InvalidArgv)?,
                    offset: (chunk_index * MAX_ARGV_CHUNK_BYTES)
                        .try_into()
                        .map_err(|_| RunnerProtocolError::InvalidArgv)?,
                    total: argument
                        .len()
                        .try_into()
                        .map_err(|_| RunnerProtocolError::InvalidArgv)?,
                    data: encode_base64(chunk),
                },
            ));
            sequence = sequence
                .checked_add(1)
                .ok_or(RunnerProtocolError::InvalidSequence)?;
        }
    }
    frames.push(ControlFrame::new(
        identity,
        sequence,
        ControlMessage::ArgvEnd,
    ));
    for frame in &frames {
        encode_frame(frame)?;
    }
    Ok(frames)
}

#[cfg(target_os = "linux")]
pub(crate) fn send_frame_packet(socket: &File, frame: &ControlFrame) -> RunnerResult<()> {
    let bytes = encode_frame(frame)?;
    let sent = unsafe {
        libc::send(
            socket.as_raw_fd(),
            bytes.as_ptr().cast(),
            bytes.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    if sent != bytes.len() as isize {
        return Err(RunnerProtocolError::Io);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_frame_packet(socket: &File) -> RunnerResult<ControlFrame> {
    let mut bytes = [0_u8; MAX_CONTROL_FRAME_BYTES];
    let received = unsafe {
        libc::recv(
            socket.as_raw_fd(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            libc::MSG_TRUNC,
        )
    };
    if received <= 0 {
        return Err(RunnerProtocolError::Io);
    }
    if received as usize > bytes.len() {
        return Err(RunnerProtocolError::OversizedFrame);
    }
    decode_frame(&bytes[..received as usize])
}

#[cfg(target_os = "linux")]
pub(crate) fn send_frame_with_one_fd(
    socket: &File,
    frame: &ControlFrame,
    transferred: &File,
) -> RunnerResult<()> {
    let bytes = encode_frame(frame)?;
    let mut iovec = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let rights_bytes = std::mem::size_of::<RawFd>();
    let control_len = unsafe { libc::CMSG_SPACE(rights_bytes as u32) } as usize;
    let mut control = vec![0_u8; control_len];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iovec;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null() {
            return Err(RunnerProtocolError::DescriptorViolation);
        }
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(rights_bytes as u32) as usize;
        std::ptr::write_unaligned(
            libc::CMSG_DATA(header).cast::<RawFd>(),
            transferred.as_raw_fd(),
        );
        message.msg_controllen = (*header).cmsg_len;
    }
    let sent = unsafe { libc::sendmsg(socket.as_raw_fd(), &message, libc::MSG_NOSIGNAL) };
    if sent != bytes.len() as isize {
        return Err(RunnerProtocolError::Io);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_frame_with_one_fd(socket: &File) -> RunnerResult<(ControlFrame, File)> {
    let mut bytes = [0_u8; MAX_CONTROL_FRAME_BYTES];
    let mut iovec = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let rights_capacity = 8 * std::mem::size_of::<RawFd>();
    let control_len = unsafe { libc::CMSG_SPACE(rights_capacity as u32) } as usize;
    let mut control = vec![0_u8; control_len];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iovec;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    let received = unsafe {
        libc::recvmsg(
            socket.as_raw_fd(),
            &mut message,
            libc::MSG_CMSG_CLOEXEC | libc::MSG_TRUNC,
        )
    };
    if received <= 0 {
        return Err(RunnerProtocolError::Io);
    }

    // Take ownership of every delivered descriptor before inspecting flags,
    // payload JSON, or ancillary shape.  All error paths therefore close all
    // kernel-delivered descriptors.
    let mut files = Vec::new();
    let mut header_count = 0_usize;
    let mut shape_valid = true;
    let mut header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    while !header.is_null() {
        header_count += 1;
        let minimum = unsafe { libc::CMSG_LEN(0) } as usize;
        let header_len = unsafe { (*header).cmsg_len };
        if header_len < minimum {
            shape_valid = false;
            break;
        }
        let is_rights = unsafe {
            (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS
        };
        let payload_len = header_len - minimum;
        if !is_rights || payload_len % std::mem::size_of::<RawFd>() != 0 {
            shape_valid = false;
        } else {
            for index in 0..(payload_len / std::mem::size_of::<RawFd>()) {
                let raw = unsafe {
                    std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>().add(index))
                };
                if raw < 0 {
                    shape_valid = false;
                } else {
                    files.push(unsafe { File::from_raw_fd(raw) });
                }
            }
        }
        header = unsafe { libc::CMSG_NXTHDR(&message, header) };
    }
    if received as usize > bytes.len()
        || message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
        || !shape_valid
        || header_count != 1
        || files.len() != 1
    {
        return Err(RunnerProtocolError::DescriptorViolation);
    }
    let frame = decode_frame(&bytes[..received as usize])?;
    Ok((frame, files.pop().expect("exactly one received descriptor")))
}

fn decode_canonical_base64(value: &str) -> RunnerResult<Vec<u8>> {
    if !value.is_ascii() || value.is_empty() || value.len() % 4 != 0 {
        return Err(RunnerProtocolError::InvalidArgv);
    }
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len() / 4 * 3);
    for (chunk_index, chunk) in bytes.chunks_exact(4).enumerate() {
        let last = chunk_index + 1 == bytes.len() / 4;
        let a = decode_base64_digit(chunk[0])?;
        let b = decode_base64_digit(chunk[1])?;
        let c = if chunk[2] == b'=' {
            None
        } else {
            Some(decode_base64_digit(chunk[2])?)
        };
        let d = if chunk[3] == b'=' {
            None
        } else {
            Some(decode_base64_digit(chunk[3])?)
        };
        if (!last && (c.is_none() || d.is_none())) || (c.is_none() && d.is_some()) {
            return Err(RunnerProtocolError::InvalidArgv);
        }
        decoded.push((a << 2) | (b >> 4));
        if let Some(c) = c {
            if d.is_none() && c & 0x03 != 0 {
                return Err(RunnerProtocolError::InvalidArgv);
            }
            decoded.push((b << 4) | (c >> 2));
            if let Some(d) = d {
                decoded.push((c << 6) | d);
            }
        } else if b & 0x0f != 0 {
            return Err(RunnerProtocolError::InvalidArgv);
        }
    }
    if encode_base64(&decoded) != value {
        return Err(RunnerProtocolError::InvalidArgv);
    }
    Ok(decoded)
}

fn decode_base64_digit(byte: u8) -> RunnerResult<u8> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(RunnerProtocolError::InvalidArgv),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> RunnerIdentity {
        RunnerIdentity {
            tournament_uuid: Uuid::from_u128(7),
            candidate_index: 1,
            generation: 9,
        }
    }

    fn placement() -> PlacementDescriptorEvidence {
        PlacementDescriptorEvidence {
            dev: 1,
            ino: 2,
            statx_mnt_id_unique: 3,
            candidate_index: 1,
            generation: 9,
        }
    }

    #[test]
    fn exact_control_sequence_accepts_only_the_approved_order() {
        let messages = vec![
            ControlMessage::Hello,
            ControlMessage::HelloAck,
            ControlMessage::ArgvBegin {
                argument_count: 1,
                total_bytes: 3,
            },
            ControlMessage::ArgvChunk {
                argument_index: 0,
                offset: 0,
                total: 3,
                data: encode_base64(b"cmd"),
            },
            ControlMessage::ArgvEnd,
            ControlMessage::Ready,
            ControlMessage::ReadyAck {
                placement: placement(),
            },
            ControlMessage::PayloadFdAck,
            ControlMessage::Go,
            ControlMessage::GoReceived { elapsed_ns: 1 },
            ControlMessage::PayloadPlaced,
            ControlMessage::PayloadRelease,
            ControlMessage::LeaderExited {
                category: RunnerExitCategory::ExitedZero,
            },
            ControlMessage::OutputDrained { bytes: 4 },
            ControlMessage::ResultAccepted,
            ControlMessage::Decision {
                decision: DecisionKind::Select,
            },
            ControlMessage::Ack {
                decision: DecisionKind::Select,
            },
        ];
        let mut validator = SequenceValidator::new(identity()).unwrap();
        for (sequence, message) in messages.into_iter().enumerate() {
            let frame = ControlFrame::new(identity(), sequence as u32, message);
            validator
                .accept(&decode_frame(&encode_frame(&frame).unwrap()).unwrap())
                .unwrap();
        }
        assert!(validator.is_complete());
    }

    #[test]
    fn sequence_rejects_duplicate_stale_wrong_identity_and_oversize_without_raw_echo() {
        let mut validator = SequenceValidator::new(identity()).unwrap();
        let hello = ControlFrame::new(identity(), 0, ControlMessage::Hello);
        validator.accept(&hello).unwrap();
        assert_eq!(
            validator.accept(&hello),
            Err(RunnerProtocolError::InvalidSequence)
        );

        let mut stale = identity();
        stale.generation += 1;
        assert_eq!(
            validator.accept(&ControlFrame::new(stale, 1, ControlMessage::HelloAck)),
            Err(RunnerProtocolError::StaleGeneration)
        );
        assert_eq!(
            decode_frame(&vec![b'x'; MAX_CONTROL_FRAME_BYTES + 1]),
            Err(RunnerProtocolError::OversizedFrame)
        );
        assert!(!RunnerProtocolError::InvalidFrame.to_string().contains('x'));
    }

    #[test]
    fn argv_chunks_round_trip_non_utf8_and_reject_offset_duplicate_and_noncanonical_base64() {
        let argv = [b"/usr/bin/tool".as_slice(), b"\xff--flag".as_slice()];
        let total = argv.iter().map(|argument| argument.len()).sum::<usize>();
        let mut assembler = ArgvAssembler::begin(argv.len() as u16, total as u32).unwrap();
        for (index, argument) in argv.iter().enumerate() {
            assembler
                .push_chunk(
                    index as u16,
                    0,
                    argument.len() as u32,
                    &encode_base64(argument),
                )
                .unwrap();
        }
        assert_eq!(assembler.finish().unwrap(), argv);

        let mut duplicate = ArgvAssembler::begin(1, 3).unwrap();
        duplicate
            .push_chunk(0, 0, 3, &encode_base64(b"ab"))
            .unwrap();
        assert_eq!(
            duplicate.push_chunk(0, 0, 3, &encode_base64(b"c")),
            Err(RunnerProtocolError::InvalidArgv)
        );
        assert_eq!(
            decode_canonical_base64("Zh=="),
            Err(RunnerProtocolError::InvalidArgv)
        );
    }

    #[test]
    fn frame_serde_rejects_unknown_fields_and_descriptor_generation_mismatch() {
        let frame = ControlFrame::new(identity(), 0, ControlMessage::Hello);
        let mut value = serde_json::to_value(frame).unwrap();
        value["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ControlFrame>(value).is_err());

        let mut bad = placement();
        bad.generation += 1;
        assert_eq!(
            bad.validate(identity()),
            Err(RunnerProtocolError::DescriptorViolation)
        );
    }
}
