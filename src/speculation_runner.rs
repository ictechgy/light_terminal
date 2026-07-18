//! Trusted speculation runner protocol and Linux-only hidden runner.
//!
//! The protocol is private, generation-bound, packet-bounded, and never
//! renders attacker-controlled bytes in errors.  Candidate PTY bytes are not
//! control input and are only counted by the Linux runner.

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fmt;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
#[cfg(target_os = "linux")]
use std::time::Instant;
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
#[serde(rename_all = "snake_case")]
pub(crate) enum PlacementDescriptorKind {
    PayloadCgroupProcs,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlacementDescriptorEvidence {
    pub kind: PlacementDescriptorKind,
    pub file_dev: u64,
    pub file_ino: u64,
    pub file_statx_mnt_id_unique: u64,
    pub payload_dev: u64,
    pub payload_ino: u64,
    pub payload_statx_mnt_id_unique: u64,
    pub candidate_index: u8,
    pub generation: u64,
}

impl PlacementDescriptorEvidence {
    pub(crate) fn validate(self, identity: RunnerIdentity) -> RunnerResult<()> {
        if self.kind != PlacementDescriptorKind::PayloadCgroupProcs
            || self.file_dev == 0
            || self.file_ino == 0
            || self.file_statx_mnt_id_unique == 0
            || self.payload_dev == 0
            || self.payload_ino == 0
            || self.payload_statx_mnt_id_unique == 0
            || self.file_statx_mnt_id_unique != self.payload_statx_mnt_id_unique
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
        monotonic_ns: u64,
    },
    PayloadPlaced,
    PayloadRelease,
    OutputLimitExceeded {
        bytes: u64,
    },
    OutputCleanupClaimed,
    LeaderExited {
        category: RunnerExitCategory,
        elapsed_ns: u64,
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
    ExecutionEvent,
    OutputCleanupClaim,
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
                SequenceState::ExecutionEvent
            }
            (SequenceState::ExecutionEvent, ControlMessage::OutputLimitExceeded { .. }) => {
                SequenceState::OutputCleanupClaim
            }
            (SequenceState::OutputCleanupClaim, ControlMessage::OutputCleanupClaimed) => {
                SequenceState::LeaderExited
            }
            (SequenceState::ExecutionEvent, ControlMessage::LeaderExited { .. })
            | (SequenceState::LeaderExited, ControlMessage::LeaderExited { .. }) => {
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

pub(crate) fn dispatch_internal_speculation_mode(arguments: &[OsString]) -> RunnerResult<bool> {
    let Some(mode) = arguments.get(1).and_then(|value| value.to_str()) else {
        return Ok(false);
    };
    if mode == "--internal-speculation-probe-v1" {
        #[cfg(target_os = "linux")]
        {
            run_internal_probe()?;
            return Ok(true);
        }
        #[cfg(not(target_os = "linux"))]
        return Err(RunnerProtocolError::Unsupported);
    }
    if mode != "--internal-speculation-runner-v1" {
        return Ok(false);
    }
    #[cfg(all(target_os = "linux", debug_assertions))]
    if std::env::var_os("LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION").as_deref()
        == Some(std::ffi::OsStr::new("exit"))
    {
        let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
        if null >= 0 {
            unsafe {
                libc::dup2(null, 2);
                libc::close(null);
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        let (identity, control) = parse_internal_runner_arguments(arguments)?;
        let result = run_internal_runner(identity, &control);
        #[cfg(debug_assertions)]
        if result.is_err()
            && std::env::var_os("LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION").as_deref()
                == Some(std::ffi::OsStr::new("exit"))
        {
            unsafe { libc::_exit(86) };
        }
        result?;
        Ok(true)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(RunnerProtocolError::Unsupported)
    }
}

#[cfg(target_os = "linux")]
fn parse_internal_runner_arguments(
    arguments: &[OsString],
) -> RunnerResult<(RunnerIdentity, OsString)> {
    if arguments.len() != 10
        || arguments[2] != "--tournament"
        || arguments[4] != "--candidate-index"
        || arguments[6] != "--generation"
        || arguments[8] != "--control"
    {
        return Err(RunnerProtocolError::InvalidFrame);
    }
    let identity = RunnerIdentity {
        tournament_uuid: arguments[3]
            .to_str()
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or(RunnerProtocolError::InvalidIdentity)?,
        candidate_index: arguments[5]
            .to_str()
            .and_then(|value| value.parse().ok())
            .ok_or(RunnerProtocolError::InvalidIdentity)?,
        generation: arguments[7]
            .to_str()
            .and_then(|value| value.parse().ok())
            .ok_or(RunnerProtocolError::InvalidIdentity)?,
    };
    identity.validate()?;
    let control = arguments[9].clone();
    let bytes = control.as_os_str().as_bytes();
    if !bytes.starts_with(b"/") || bytes.is_empty() || bytes.len() >= 108 || bytes.contains(&0) {
        return Err(RunnerProtocolError::InvalidFrame);
    }
    Ok((identity, control))
}

#[cfg(target_os = "linux")]
fn run_internal_runner(
    identity: RunnerIdentity,
    control_path: &std::ffi::OsStr,
) -> RunnerResult<()> {
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(RunnerProtocolError::Io);
    }
    unsafe { libc::close(10) };
    let peer = connect_seqpacket(control_path)?;
    let mut validator = SequenceValidator::new(identity)?;
    send_runner_frame(&peer, &mut validator, identity, ControlMessage::Hello)?;
    let hello_ack = receive_runner_frame(&peer, &mut validator)?;
    if !matches!(hello_ack.message, ControlMessage::HelloAck) {
        return Err(RunnerProtocolError::InvalidSequence);
    }
    let begin = receive_runner_frame(&peer, &mut validator)?;
    let ControlMessage::ArgvBegin {
        argument_count,
        total_bytes,
    } = begin.message
    else {
        return Err(RunnerProtocolError::InvalidSequence);
    };
    let mut assembler = ArgvAssembler::begin(argument_count, total_bytes)?;
    loop {
        let frame = receive_runner_frame(&peer, &mut validator)?;
        match frame.message {
            ControlMessage::ArgvChunk {
                argument_index,
                offset,
                total,
                data,
            } => assembler.push_chunk(argument_index, offset, total, &data)?,
            ControlMessage::ArgvEnd => break,
            _ => return Err(RunnerProtocolError::InvalidSequence),
        }
    }
    let argv = assembler.finish()?;
    let pty = InternalPty::open()?;
    send_runner_frame(&peer, &mut validator, identity, ControlMessage::Ready)?;
    runner_failpoint("runner_before_payload_fd_receive")?;
    let (ready_ack, placement) = receive_frame_with_one_fd(&peer)?;
    runner_failpoint("runner_after_payload_fd_receive")?;
    validator.accept(&ready_ack)?;
    let ControlMessage::ReadyAck {
        placement: expected,
    } = ready_ack.message
    else {
        return Err(RunnerProtocolError::DescriptorViolation);
    };
    runner_failpoint("runner_before_payload_fd_validation")?;
    validate_placement_fd(&placement, expected, identity)?;
    runner_failpoint("runner_after_payload_fd_validation")?;
    runner_failpoint("runner_before_payload_fd_ack")?;
    let payload_fd_ack_sequence = validator.next_sequence();
    send_runner_frame(
        &peer,
        &mut validator,
        identity,
        ControlMessage::PayloadFdAck,
    )?;
    runner_failpoint("runner_after_payload_fd_ack")?;
    if runner_failpoint_enabled("runner_duplicate_payload_fd_ack") {
        let duplicate = ControlFrame::new(
            identity,
            payload_fd_ack_sequence,
            ControlMessage::PayloadFdAck,
        );
        send_frame_packet(&peer, &duplicate)?;
    }
    let go = receive_runner_frame(&peer, &mut validator)?;
    if !matches!(go.message, ControlMessage::Go) {
        return Err(RunnerProtocolError::InvalidSequence);
    }
    let go_started = Instant::now();
    send_runner_frame(
        &peer,
        &mut validator,
        identity,
        ControlMessage::GoReceived {
            monotonic_ns: monotonic_now_ns()?,
        },
    )?;
    runner_failpoint("runner_before_candidate_fork")?;
    let child = spawn_placement_blocked_child(
        placement,
        &pty,
        &argv,
        RunnerChildFailpoints::from_environment(),
    )?;
    runner_failpoint("runner_after_candidate_fork")?;
    drop(pty.slave);
    drop(pty.dev_null);
    wait_for_placed_ack(&child.placed_read)?;
    runner_failpoint("runner_before_payload_placed_send")?;
    send_runner_frame(
        &peer,
        &mut validator,
        identity,
        ControlMessage::PayloadPlaced,
    )?;
    runner_failpoint("runner_after_payload_placed_send")?;
    runner_failpoint("runner_before_release_receive")?;
    let release = receive_runner_frame(&peer, &mut validator)?;
    runner_failpoint("runner_after_release_receive")?;
    if !matches!(release.message, ControlMessage::PayloadRelease) {
        return Err(RunnerProtocolError::InvalidSequence);
    }
    release_child(&child.release_write)?;
    let mut bytes = 0;
    let mut overflow_reported = false;
    let completion = loop {
        match drain_until_execution_event(
            child.pid,
            &pty.master,
            go_started,
            bytes,
            overflow_reported,
        )? {
            ExecutionProgress::OutputLimit {
                total_bytes,
                reported_bytes,
            } => {
                bytes = total_bytes;
                overflow_reported = true;
                send_runner_frame(
                    &peer,
                    &mut validator,
                    identity,
                    ControlMessage::OutputLimitExceeded {
                        bytes: reported_bytes,
                    },
                )?;
                let cleanup = receive_runner_frame(&peer, &mut validator)?;
                if !matches!(cleanup.message, ControlMessage::OutputCleanupClaimed) {
                    return Err(RunnerProtocolError::InvalidSequence);
                }
            }
            ExecutionProgress::LeaderExited(completion) => break completion,
        }
    };
    send_runner_frame(
        &peer,
        &mut validator,
        identity,
        ControlMessage::LeaderExited {
            category: completion.category,
            elapsed_ns: completion.elapsed_ns,
        },
    )?;
    // The daemon receives LEADER_EXITED before this read reaches EOF and may
    // now kill the payload cgroup.  Descendants retaining the PTY slave cannot
    // make output-drained substitute for cgroup emptiness.
    let bytes = drain_to_eof(&pty.master, completion.bytes)?;
    send_runner_frame(
        &peer,
        &mut validator,
        identity,
        ControlMessage::OutputDrained { bytes },
    )?;
    let accepted = receive_runner_frame(&peer, &mut validator)?;
    if !matches!(accepted.message, ControlMessage::ResultAccepted) {
        return Err(RunnerProtocolError::InvalidSequence);
    }
    let decision = receive_runner_frame(&peer, &mut validator)?;
    let ControlMessage::Decision { decision } = decision.message else {
        return Err(RunnerProtocolError::InvalidSequence);
    };
    send_runner_frame(
        &peer,
        &mut validator,
        identity,
        ControlMessage::Ack { decision },
    )?;
    if !validator.is_complete() {
        return Err(RunnerProtocolError::InvalidSequence);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn run_internal_probe() -> RunnerResult<()> {
    let expected = [
        ("PATH", "/usr/bin:/bin"),
        ("HOME", "/home/lterm"),
        ("TMPDIR", "/tmp"),
        ("LANG", "C.UTF-8"),
        ("LC_ALL", "C.UTF-8"),
        ("TERM", "xterm-256color"),
    ];
    for (name, value) in expected {
        if std::env::var(name).as_deref() != Ok(value) {
            return Err(RunnerProtocolError::InvalidFrame);
        }
    }
    if std::env::vars_os().count() != expected.len() {
        return Err(RunnerProtocolError::InvalidFrame);
    }
    let status = std::fs::read("/proc/self/status").map_err(|_| RunnerProtocolError::Io)?;
    let status = std::str::from_utf8(&status).map_err(|_| RunnerProtocolError::InvalidFrame)?;
    if !status.lines().any(|line| line == "NoNewPrivs:\t1")
        || !status
            .lines()
            .any(|line| line == "CapEff:\t0000000000000000")
    {
        return Err(RunnerProtocolError::InvalidFrame);
    }
    if std::path::Path::new("/sys").exists()
        || std::path::Path::new("/sys/fs/cgroup").exists()
        || std::path::Path::new("/run/docker.sock").exists()
        || std::path::Path::new("/run/podman/podman.sock").exists()
        || std::path::Path::new("/var/run/docker.sock").exists()
        || std::path::Path::new("/run/dbus/system_bus_socket").exists()
        || std::path::Path::new("/run/lterm-control/control.sock").exists()
    {
        return Err(RunnerProtocolError::InvalidFrame);
    }
    let mut stdin_byte = 0_u8;
    if unsafe { libc::read(0, (&mut stdin_byte as *mut u8).cast(), 1) } != 0
        || unsafe { libc::isatty(1) } != 1
        || unsafe { libc::isatty(2) } != 1
    {
        return Err(RunnerProtocolError::InvalidFrame);
    }
    let controlling_tty = unsafe {
        libc::open(
            c"/dev/tty".as_ptr(),
            libc::O_RDONLY | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if controlling_tty >= 0 {
        unsafe { libc::close(controlling_tty) };
        return Err(RunnerProtocolError::InvalidFrame);
    }
    for fd in [0, 1, 2] {
        if unsafe { libc::tcgetsid(fd) } >= 0 {
            return Err(RunnerProtocolError::InvalidFrame);
        }
    }
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } == 0 {
        return Err(RunnerProtocolError::InvalidFrame);
    }
    prove_no_external_ip_connectivity()?;
    audit_probe_descriptors()?;
    let canary = std::path::Path::new("/workspace/.lterm-speculation-probe-v1");
    std::fs::write(canary, b"probe").map_err(|_| RunnerProtocolError::Io)?;
    std::fs::remove_file(canary).map_err(|_| RunnerProtocolError::Io)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn prove_no_external_ip_connectivity() -> RunnerResult<()> {
    let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if socket < 0 {
        return Err(RunnerProtocolError::Io);
    }
    let address = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: u16::to_be(9),
        sin_addr: libc::in_addr {
            s_addr: u32::from_be_bytes([198, 51, 100, 1]).to_be(),
        },
        sin_zero: [0; 8],
    };
    let connected = unsafe {
        libc::connect(
            socket,
            (&address as *const libc::sockaddr_in).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    } == 0;
    unsafe { libc::close(socket) };
    if connected {
        return Err(RunnerProtocolError::InvalidFrame);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn audit_probe_descriptors() -> RunnerResult<()> {
    let directory = std::fs::read_dir("/proc/self/fd").map_err(|_| RunnerProtocolError::Io)?;
    let mut enumeration_fds = 0_u8;
    for entry in directory {
        let entry = entry.map_err(|_| RunnerProtocolError::Io)?;
        let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<RawFd>().ok())
        else {
            return Err(RunnerProtocolError::InvalidFrame);
        };
        if fd < 3 {
            continue;
        }
        let target = std::fs::read_link(entry.path()).map_err(|_| RunnerProtocolError::Io)?;
        let target = target.as_os_str().as_bytes();
        if target.starts_with(b"/proc/") && target.ends_with(b"/fd") {
            enumeration_fds = enumeration_fds.saturating_add(1);
            continue;
        }
        return Err(RunnerProtocolError::InvalidFrame);
    }
    if enumeration_fds > 1 {
        return Err(RunnerProtocolError::InvalidFrame);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn connect_seqpacket(path: &std::ffi::OsStr) -> RunnerResult<File> {
    let bytes = path.as_bytes();
    let mut address = std::mem::MaybeUninit::<libc::sockaddr_un>::zeroed();
    let address_mut = unsafe { address.assume_init_mut() };
    if bytes.len() >= address_mut.sun_path.len() {
        return Err(RunnerProtocolError::InvalidFrame);
    }
    address_mut.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, source) in address_mut.sun_path.iter_mut().zip(bytes) {
        *target = *source as libc::c_char;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(RunnerProtocolError::Unsupported);
    }
    let socket = unsafe { File::from_raw_fd(fd) };
    let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1)
        .try_into()
        .map_err(|_| RunnerProtocolError::InvalidFrame)?;
    if unsafe {
        libc::connect(
            socket.as_raw_fd(),
            address_mut as *const libc::sockaddr_un as *const libc::sockaddr,
            length,
        )
    } != 0
    {
        return Err(RunnerProtocolError::Io);
    }
    Ok(socket)
}

#[cfg(target_os = "linux")]
fn send_runner_frame(
    peer: &File,
    validator: &mut SequenceValidator,
    identity: RunnerIdentity,
    message: ControlMessage,
) -> RunnerResult<()> {
    let frame = ControlFrame::new(identity, validator.next_sequence(), message);
    validator.accept(&frame)?;
    send_frame_packet(peer, &frame)
}

#[cfg(target_os = "linux")]
fn receive_runner_frame(
    peer: &File,
    validator: &mut SequenceValidator,
) -> RunnerResult<ControlFrame> {
    let frame = receive_frame_packet(peer)?;
    validator.accept(&frame)?;
    Ok(frame)
}

#[cfg(target_os = "linux")]
fn runner_failpoint(name: &str) -> RunnerResult<()> {
    if runner_failpoint_enabled(name) {
        #[cfg(debug_assertions)]
        if std::env::var_os("LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION").as_deref()
            == Some(std::ffi::OsStr::new("exit"))
        {
            unsafe { libc::_exit(86) };
        }
        return Err(RunnerProtocolError::Io);
    }
    Ok(())
}

#[cfg(all(target_os = "linux", debug_assertions))]
const RUNNER_FAILPOINTS: &[&str] = &[
    "runner_before_payload_fd_receive",
    "runner_after_payload_fd_receive",
    "runner_before_payload_fd_validation",
    "runner_after_payload_fd_validation",
    "runner_before_payload_fd_ack",
    "runner_after_payload_fd_ack",
    "runner_duplicate_payload_fd_ack",
    "runner_before_candidate_fork",
    "runner_after_candidate_fork",
    "runner_before_child_placement",
    "runner_after_child_placement",
    "runner_before_payload_placed_send",
    "runner_after_payload_placed_send",
    "runner_before_release_receive",
    "runner_after_release_receive",
    "runner_before_child_exec",
];

#[cfg(target_os = "linux")]
fn runner_failpoint_enabled(name: &str) -> bool {
    #[cfg(debug_assertions)]
    {
        RUNNER_FAILPOINTS.contains(&name)
            && std::env::var("LTERM_INTERNAL_SPECULATION_RUNNER_FAILPOINT").as_deref() == Ok(name)
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = name;
        false
    }
}

#[cfg(target_os = "linux")]
struct InternalPty {
    master: File,
    slave: File,
    dev_null: File,
}

#[cfg(target_os = "linux")]
impl InternalPty {
    fn open() -> RunnerResult<Self> {
        let master_fd =
            unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
        if master_fd < 0
            || unsafe { libc::grantpt(master_fd) } != 0
            || unsafe { libc::unlockpt(master_fd) } != 0
        {
            if master_fd >= 0 {
                unsafe { libc::close(master_fd) };
            }
            return Err(RunnerProtocolError::Io);
        }
        let master = unsafe { File::from_raw_fd(master_fd) };
        let mut name = [0 as libc::c_char; 256];
        if unsafe { libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) } != 0 {
            return Err(RunnerProtocolError::Io);
        }
        if !name.contains(&0) {
            return Err(RunnerProtocolError::Io);
        }
        let slave_fd = unsafe {
            libc::open(
                name.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
            )
        };
        if slave_fd < 0 {
            return Err(RunnerProtocolError::Io);
        }
        let dev_null_fd = unsafe {
            libc::open(
                c"/dev/null".as_ptr(),
                libc::O_RDONLY | libc::O_NOCTTY | libc::O_CLOEXEC,
            )
        };
        if dev_null_fd < 0 {
            unsafe { libc::close(slave_fd) };
            return Err(RunnerProtocolError::Io);
        }
        Ok(Self {
            master,
            slave: unsafe { File::from_raw_fd(slave_fd) },
            dev_null: unsafe { File::from_raw_fd(dev_null_fd) },
        })
    }
}

#[cfg(target_os = "linux")]
struct PlacementBlockedChild {
    pid: libc::pid_t,
    placed_read: File,
    release_write: File,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct RunnerChildFailpoints {
    before_placement: bool,
    after_placement: bool,
    before_exec: bool,
}

#[cfg(target_os = "linux")]
impl RunnerChildFailpoints {
    fn from_environment() -> Self {
        Self {
            before_placement: runner_failpoint_enabled("runner_before_child_placement"),
            after_placement: runner_failpoint_enabled("runner_after_child_placement"),
            before_exec: runner_failpoint_enabled("runner_before_child_exec"),
        }
    }
}

#[cfg(target_os = "linux")]
fn spawn_placement_blocked_child(
    placement: File,
    pty: &InternalPty,
    arguments: &[Vec<u8>],
    failpoints: RunnerChildFailpoints,
) -> RunnerResult<PlacementBlockedChild> {
    let argv = arguments
        .iter()
        .map(|argument| std::ffi::CString::new(argument.as_slice()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| RunnerProtocolError::InvalidArgv)?;
    let env = [
        c"PATH=/usr/bin:/bin",
        c"HOME=/home/lterm",
        c"TMPDIR=/tmp",
        c"LANG=C.UTF-8",
        c"LC_ALL=C.UTF-8",
        c"TERM=xterm-256color",
    ];
    let mut argv_ptrs = argv
        .iter()
        .map(|argument| argument.as_ptr())
        .collect::<Vec<_>>();
    argv_ptrs.push(std::ptr::null());
    let mut env_ptrs = env.iter().map(|value| value.as_ptr()).collect::<Vec<_>>();
    env_ptrs.push(std::ptr::null());
    let placement = relocate_high(placement)?;
    let (placed_read, placed_write) = cloexec_pipe()?;
    let placed_write = relocate_high(placed_write)?;
    let (release_read, release_write) = cloexec_pipe()?;
    let release_read = relocate_high(release_read)?;
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(RunnerProtocolError::Io);
    }
    if pid == 0 {
        unsafe {
            if libc::dup2(pty.dev_null.as_raw_fd(), 0) < 0
                || libc::dup2(pty.slave.as_raw_fd(), 1) < 0
                || libc::dup2(pty.slave.as_raw_fd(), 2) < 0
                || libc::dup3(placement.as_raw_fd(), 3, libc::O_CLOEXEC) < 0
                || libc::dup3(placed_write.as_raw_fd(), 4, libc::O_CLOEXEC) < 0
                || libc::dup3(release_read.as_raw_fd(), 5, libc::O_CLOEXEC) < 0
            {
                libc::_exit(125);
            }
            close_child_fds_from(6);
            if failpoints.before_placement {
                libc::_exit(124);
            }
            if libc::write(3, b"0\n".as_ptr().cast(), 2) != 2 {
                libc::_exit(125);
            }
            libc::close(3);
            if failpoints.after_placement {
                libc::_exit(124);
            }
            if libc::write(4, b"P".as_ptr().cast(), 1) != 1 {
                libc::_exit(125);
            }
            libc::close(4);
            let mut release = 0_u8;
            if libc::read(5, (&mut release as *mut u8).cast(), 1) != 1 || release != b'R' {
                libc::_exit(125);
            }
            libc::close(5);
            if failpoints.before_exec {
                libc::_exit(124);
            }
            libc::execve(argv_ptrs[0], argv_ptrs.as_ptr(), env_ptrs.as_ptr());
            libc::_exit(126);
        }
    }
    drop((placement, placed_write, release_read));
    Ok(PlacementBlockedChild {
        pid,
        placed_read,
        release_write,
    })
}

#[cfg(target_os = "linux")]
fn relocate_high(file: File) -> RunnerResult<File> {
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 64) };
    if fd < 64 {
        return Err(RunnerProtocolError::Io);
    }
    drop(file);
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn cloexec_pipe() -> RunnerResult<(File, File)> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(RunnerProtocolError::Io);
    }
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

#[cfg(target_os = "linux")]
unsafe fn close_child_fds_from(first: RawFd) {
    if unsafe { libc::syscall(libc::SYS_close_range, first as libc::c_uint, u32::MAX, 0) } == 0 {
        return;
    }
    for fd in first..65_536 {
        unsafe { libc::close(fd) };
    }
}

#[cfg(target_os = "linux")]
fn wait_for_placed_ack(file: &File) -> RunnerResult<()> {
    let mut byte = 0_u8;
    let read = unsafe { libc::read(file.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
    if read != 1 || byte != b'P' {
        return Err(RunnerProtocolError::Io);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn release_child(file: &File) -> RunnerResult<()> {
    if unsafe { libc::write(file.as_raw_fd(), b"R".as_ptr().cast(), 1) } != 1 {
        return Err(RunnerProtocolError::Io);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
struct LeaderCompletion {
    category: RunnerExitCategory,
    bytes: u64,
    elapsed_ns: u64,
}

#[cfg(target_os = "linux")]
enum ExecutionProgress {
    OutputLimit {
        total_bytes: u64,
        reported_bytes: u64,
    },
    LeaderExited(LeaderCompletion),
}

#[cfg(target_os = "linux")]
fn drain_until_execution_event(
    pid: libc::pid_t,
    master: &File,
    started: Instant,
    mut bytes: u64,
    overflow_reported: bool,
) -> RunnerResult<ExecutionProgress> {
    set_nonblocking(master)?;
    loop {
        let (next, _, overflow) = drain_available_with_eof(master, bytes)?;
        bytes = next;
        if !overflow_reported && let Some(reported_bytes) = overflow {
            return Ok(ExecutionProgress::OutputLimit {
                total_bytes: bytes,
                reported_bytes,
            });
        }
        let mut status = 0_i32;
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if waited == pid {
            let category =
                if overflow_reported || bytes > crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES {
                    RunnerExitCategory::OutputLimitExceeded
                } else if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
                    RunnerExitCategory::ExitedZero
                } else if libc::WIFEXITED(status) {
                    RunnerExitCategory::ExitedNonzero
                } else if libc::WIFSIGNALED(status) {
                    RunnerExitCategory::Signaled
                } else {
                    RunnerExitCategory::EvidenceIncomplete
                };
            return Ok(ExecutionProgress::LeaderExited(LeaderCompletion {
                category,
                bytes,
                elapsed_ns: started.elapsed().as_nanos().try_into().unwrap_or(u64::MAX),
            }));
        }
        if waited < 0 {
            return Err(RunnerProtocolError::Io);
        }
        let mut pollfd = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        unsafe { libc::poll(&mut pollfd, 1, 10) };
    }
}

#[cfg(target_os = "linux")]
fn drain_to_eof(master: &File, mut bytes: u64) -> RunnerResult<u64> {
    loop {
        match drain_available_with_eof(master, bytes)? {
            (next, true, _) => return Ok(next),
            (next, false, _) => bytes = next,
        }
        let mut pollfd = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pollfd, 1, 5_000) } <= 0 {
            return Err(RunnerProtocolError::Io);
        }
    }
}

#[cfg(target_os = "linux")]
fn drain_available_with_eof(
    master: &File,
    mut bytes: u64,
) -> RunnerResult<(u64, bool, Option<u64>)> {
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read =
            unsafe { libc::read(master.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if read > 0 {
            let (next, overflow) = account_output_read(bytes, read as u64);
            bytes = next;
            if overflow.is_some() {
                return Ok((bytes, false, overflow));
            }
            continue;
        }
        if read == 0 {
            return Ok((bytes, true, None));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok((bytes, false, None));
        }
        if error.raw_os_error() == Some(libc::EIO) {
            return Ok((bytes, true, None));
        }
        return Err(RunnerProtocolError::Io);
    }
}

fn account_output_read(current: u64, read: u64) -> (u64, Option<u64>) {
    let next = current.saturating_add(read);
    let limit = crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES;
    let first_excess = (current <= limit && next > limit).then_some(limit + 1);
    (next, first_excess)
}

#[cfg(target_os = "linux")]
fn monotonic_now_ns() -> RunnerResult<u64> {
    let mut now = std::mem::MaybeUninit::<libc::timespec>::zeroed();
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, now.as_mut_ptr()) } != 0 {
        return Err(RunnerProtocolError::Io);
    }
    let now = unsafe { now.assume_init() };
    if now.tv_sec < 0 || now.tv_nsec < 0 {
        return Err(RunnerProtocolError::Io);
    }
    u64::try_from(now.tv_sec)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000_000_000))
        .and_then(|seconds| seconds.checked_add(now.tv_nsec as u64))
        .ok_or(RunnerProtocolError::Io)
}

#[cfg(target_os = "linux")]
fn set_nonblocking(file: &File) -> RunnerResult<()> {
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } != 0
    {
        return Err(RunnerProtocolError::Io);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_placement_fd(
    file: &File,
    expected: PlacementDescriptorEvidence,
    identity: RunnerIdentity,
) -> RunnerResult<()> {
    expected.validate(identity)?;
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 || !matches!(flags & libc::O_ACCMODE, libc::O_WRONLY | libc::O_RDWR) {
        return Err(RunnerProtocolError::DescriptorViolation);
    }
    let metadata = file
        .metadata()
        .map_err(|_| RunnerProtocolError::DescriptorViolation)?;
    let mut statfs = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    if unsafe { libc::fstatfs(file.as_raw_fd(), statfs.as_mut_ptr()) } != 0
        || unsafe { statfs.assume_init() }.f_type as u64 != libc::CGROUP2_SUPER_MAGIC as u64
        || metadata.dev() != expected.file_dev
        || metadata.ino() != expected.file_ino
    {
        return Err(RunnerProtocolError::DescriptorViolation);
    }
    const STATX_MNT_ID_UNIQUE: u32 = 0x0000_4000;
    let mut statx = std::mem::MaybeUninit::<libc::statx>::zeroed();
    if unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_BASIC_STATS | STATX_MNT_ID_UNIQUE,
            statx.as_mut_ptr(),
        )
    } != 0
    {
        return Err(RunnerProtocolError::DescriptorViolation);
    }
    let statx = unsafe { statx.assume_init() };
    if statx.stx_mask & STATX_MNT_ID_UNIQUE == 0
        || statx.stx_mnt_id != expected.file_statx_mnt_id_unique
    {
        return Err(RunnerProtocolError::DescriptorViolation);
    }
    validate_placement_fd_name(file)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_placement_fd_name(file: &File) -> RunnerResult<()> {
    let target = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .map_err(|_| RunnerProtocolError::DescriptorViolation)?;
    if target.file_name() != Some(std::ffi::OsStr::new("cgroup.procs"))
        || target.parent().and_then(std::path::Path::file_name)
            != Some(std::ffi::OsStr::new("payload"))
    {
        return Err(RunnerProtocolError::DescriptorViolation);
    }
    Ok(())
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
            kind: PlacementDescriptorKind::PayloadCgroupProcs,
            file_dev: 4,
            file_ino: 2,
            file_statx_mnt_id_unique: 6,
            payload_dev: 4,
            payload_ino: 5,
            payload_statx_mnt_id_unique: 6,
            candidate_index: 1,
            generation: 9,
        }
    }

    #[test]
    fn output_limit_boundary_reports_the_first_excess_byte_immediately() {
        let limit = crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES;
        assert_eq!(account_output_read(limit - 1, 1), (limit, None));
        assert_eq!(account_output_read(limit, 1), (limit + 1, Some(limit + 1)));
        assert_eq!(
            account_output_read(limit - 4, 16),
            (limit + 12, Some(limit + 1))
        );
        assert_eq!(account_output_read(limit + 1, 16), (limit + 17, None));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runner_failpoints_cover_ancillary_fork_placement_release_and_exec_seams() {
        let names = RUNNER_FAILPOINTS
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(names.len(), RUNNER_FAILPOINTS.len());
        for seam in [
            "payload_fd_receive",
            "payload_fd_validation",
            "candidate_fork",
            "child_placement",
            "payload_placed_send",
            "release_receive",
        ] {
            assert!(names.contains(format!("runner_before_{seam}").as_str()));
            assert!(names.contains(format!("runner_after_{seam}").as_str()));
        }
        assert!(names.contains("runner_before_payload_fd_ack"));
        assert!(names.contains("runner_after_payload_fd_ack"));
        assert!(names.contains("runner_duplicate_payload_fd_ack"));
        assert!(names.contains("runner_before_child_exec"));
    }

    #[test]
    fn placement_receipt_requires_exact_payload_and_cgroup_procs_binding() {
        placement().validate(identity()).unwrap();

        let mut missing_payload = placement();
        missing_payload.payload_ino = 0;
        assert_eq!(
            missing_payload.validate(identity()),
            Err(RunnerProtocolError::DescriptorViolation)
        );

        let mut wrong_kind = placement();
        wrong_kind.kind = PlacementDescriptorKind::Other;
        assert_eq!(
            wrong_kind.validate(identity()),
            Err(RunnerProtocolError::DescriptorViolation)
        );
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
            ControlMessage::GoReceived { monotonic_ns: 1 },
            ControlMessage::PayloadPlaced,
            ControlMessage::PayloadRelease,
            ControlMessage::LeaderExited {
                category: RunnerExitCategory::ExitedZero,
                elapsed_ns: 2,
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
    fn output_overflow_requires_cleanup_claim_before_leader_completion() {
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
            ControlMessage::GoReceived { monotonic_ns: 1 },
            ControlMessage::PayloadPlaced,
            ControlMessage::PayloadRelease,
            ControlMessage::OutputLimitExceeded {
                bytes: crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES + 1,
            },
            ControlMessage::OutputCleanupClaimed,
            ControlMessage::LeaderExited {
                category: RunnerExitCategory::OutputLimitExceeded,
                elapsed_ns: 2,
            },
            ControlMessage::OutputDrained {
                bytes: crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES + 1,
            },
            ControlMessage::ResultAccepted,
            ControlMessage::Decision {
                decision: DecisionKind::Abort,
            },
            ControlMessage::Ack {
                decision: DecisionKind::Abort,
            },
        ];
        let mut validator = SequenceValidator::new(identity()).unwrap();
        for (sequence, message) in messages.into_iter().enumerate() {
            validator
                .accept(&ControlFrame::new(identity(), sequence as u32, message))
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

    #[cfg(target_os = "linux")]
    fn seqpacket_pair() -> (File, File) {
        let mut fds = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    }

    #[cfg(target_os = "linux")]
    fn raw_send_with_fds(socket: &File, bytes: &[u8], files: &[&File]) {
        let mut iovec = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let payload_bytes = files.len() * std::mem::size_of::<RawFd>();
        let mut control = vec![0_u8; unsafe { libc::CMSG_SPACE(payload_bytes as u32) } as usize];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iovec;
        message.msg_iovlen = 1;
        if !files.is_empty() {
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = control.len();
            unsafe {
                let header = libc::CMSG_FIRSTHDR(&message);
                assert!(!header.is_null());
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN(payload_bytes as u32) as usize;
                for (index, file) in files.iter().enumerate() {
                    std::ptr::write_unaligned(
                        libc::CMSG_DATA(header).cast::<RawFd>().add(index),
                        file.as_raw_fd(),
                    );
                }
            }
        }
        assert_eq!(
            unsafe { libc::sendmsg(socket.as_raw_fd(), &message, libc::MSG_NOSIGNAL) },
            bytes.len() as isize
        );
    }

    #[cfg(target_os = "linux")]
    fn open_fd_count() -> usize {
        std::fs::read_dir("/proc/self/fd").unwrap().count()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ancillary_receive_accepts_exactly_one_cloexec_fd() {
        let (sender, receiver) = seqpacket_pair();
        let transferred = File::open("/dev/null").unwrap();
        let frame = ControlFrame::new(identity(), 0, ControlMessage::Hello);
        send_frame_with_one_fd(&sender, &frame, &transferred).unwrap();
        let (observed, received) = receive_frame_with_one_fd(&receiver).unwrap();
        assert_eq!(observed, frame);
        let descriptor_flags = unsafe { libc::fcntl(received.as_raw_fd(), libc::F_GETFD) };
        assert!(descriptor_flags >= 0 && descriptor_flags & libc::FD_CLOEXEC != 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ancillary_rejections_close_missing_extra_malformed_and_truncated_descriptors() {
        let frame = ControlFrame::new(identity(), 0, ControlMessage::Hello);
        let encoded = encode_frame(&frame).unwrap();
        let transferred = File::open("/dev/null").unwrap();

        let (sender, receiver) = seqpacket_pair();
        let before = open_fd_count();
        send_frame_packet(&sender, &frame).unwrap();
        assert!(matches!(
            receive_frame_with_one_fd(&receiver),
            Err(RunnerProtocolError::DescriptorViolation)
        ));
        assert_eq!(open_fd_count(), before);

        let (sender, receiver) = seqpacket_pair();
        let before = open_fd_count();
        raw_send_with_fds(&sender, &encoded, &[&transferred, &transferred]);
        assert!(matches!(
            receive_frame_with_one_fd(&receiver),
            Err(RunnerProtocolError::DescriptorViolation)
        ));
        assert_eq!(open_fd_count(), before);

        let (sender, receiver) = seqpacket_pair();
        let before = open_fd_count();
        raw_send_with_fds(&sender, b"{", &[&transferred]);
        assert!(matches!(
            receive_frame_with_one_fd(&receiver),
            Err(RunnerProtocolError::InvalidFrame)
        ));
        assert_eq!(open_fd_count(), before);

        let (sender, receiver) = seqpacket_pair();
        let before = open_fd_count();
        raw_send_with_fds(
            &sender,
            &vec![b'x'; MAX_CONTROL_FRAME_BYTES + 1],
            &[&transferred],
        );
        assert!(matches!(
            receive_frame_with_one_fd(&receiver),
            Err(RunnerProtocolError::DescriptorViolation)
        ));
        assert_eq!(open_fd_count(), before);
    }
}
