use serde::{Deserialize, Serialize};

use crate::{ByeReason, CursorShape, DisplayId, DisplayInfo, VideoConfig};

/// Identifies a file the host has offered this session. The host only ever
/// serves ids it issued, never paths from the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileId(pub u64);

impl FileId {
    /// Ids stay below 2^53 so they survive as JSON numbers in the browser.
    pub const MAX: u64 = (1 << 53) - 1;
}

/// Most file bytes in one message on the `file` channel.
pub const FILE_CHUNK_MAX: usize = 16 * 1024;
/// Bytes of a file chunk's header: the [`FileId`] then the offset of the
/// first byte, both big-endian `u64`.
pub const FILE_CHUNK_HEADER: usize = 16;
/// Most bytes the sender keeps in flight past the receiver's last ack
/// ([`ClientControl::FileAck`]).
pub const FILE_WINDOW: u64 = 1024 * 1024;

/// One binary message on the `file` channel: header, then the bytes of the
/// file from `offset` on. The offset lets the receiver drop bytes left over
/// from a transfer it cancelled.
pub fn file_chunk(id: FileId, offset: u64, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FILE_CHUNK_HEADER + data.len());
    out.extend_from_slice(&id.0.to_be_bytes());
    out.extend_from_slice(&offset.to_be_bytes());
    out.extend_from_slice(data);
    out
}

/// The parts of a [`file_chunk`]; `None` if it is too short.
pub fn parse_file_chunk(msg: &[u8]) -> Option<(FileId, u64, &[u8])> {
    let id = u64::from_be_bytes(msg.get(..8)?.try_into().ok()?);
    let offset = u64::from_be_bytes(msg.get(8..16)?.try_into().ok()?);
    Some((FileId(id), offset, &msg[FILE_CHUNK_HEADER..]))
}

/// Messages on the reliable `control` data channel, browser -> host.
///
/// Everything here is untrusted input: the host must validate or clamp every
/// value (see [`VideoConfig::clamped`]) before acting on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ClientControl {
    ListDisplays,
    SelectDisplay {
        id: DisplayId,
    },
    /// Ask for an IDR, e.g. after the decoder saw a gap.
    RequestKeyframe,
    SetVideoConfig {
        config: VideoConfig,
    },
    SetMicEnabled {
        enabled: bool,
    },
    /// Game audio quality. `None` is the host's own setting
    /// (`--audio-kbps`, `--audio-frame-ms`). Bitrates are clamped to
    /// 32-510 kb/s; frames are 10 or 20 ms.
    SetAudioConfig {
        bitrate_kbps: Option<u32>,
        frame_ms: Option<u32>,
    },
    /// Change the streamed display's desktop resolution to the largest it
    /// supports within `width` x `height` (the browser's screen). Only
    /// honoured when the host user started the host with
    /// `--allow-resolution-change`; undone on disconnect.
    SetHostResolution {
        width: u32,
        height: u32,
    },
    /// Put every display this session changed back to its own resolution.
    RestoreHostResolution,
    /// Ask for a file the host offered with [`HostControl::FileOffer`]; any
    /// other id is rejected. The bytes come on the `file` channel as
    /// [`file_chunk`]s from offset 0; the file is done when the browser has
    /// the offer's `size`. Asking again starts over.
    RequestFile {
        id: FileId,
    },
    /// The browser has stored the first `received` bytes of a requested
    /// file. The host sends at most [`FILE_WINDOW`] past it, and counts the
    /// file as sent when `received` is its size.
    FileAck {
        id: FileId,
        received: u64,
    },
    /// Stop sending a requested file. It stays on offer.
    CancelFile {
        id: FileId,
    },
    /// Send the host a file, if it accepts them
    /// ([`HostControl::Uploads`]). `id` is the browser's own, new for each
    /// file; `name` is only a suggestion, which the host cleans. After
    /// [`HostControl::UploadAccepted`] the bytes go on the `file` channel as
    /// [`file_chunk`]s from offset 0, at most [`FILE_WINDOW`] past the
    /// host's last [`HostControl::UploadAck`]. One file at a time.
    UploadFile {
        id: FileId,
        name: String,
        size: u64,
    },
    /// Stop sending a file to the host; it deletes what it has.
    CancelUpload {
        id: FileId,
    },
    /// Whether to send [`HostControl::CursorPosition`]: the browser draws
    /// the host pointer itself in relative mouse mode, while in absolute
    /// mode its own pointer is already where the host's is. Off until asked.
    SetCursorPositions {
        enabled: bool,
    },
    Ping {
        client_time_ms: f64,
    },
    /// Ctrl+Alt+Del, which no browser can send as keys: the host asks
    /// Windows for the secure attention sequence (sign-in and lock screen,
    /// Task Manager menu). Only a host running as the web-stream service
    /// can; any other answers with [`HostControl::Error`].
    SecureAttention,
    /// Update the host to the latest `main` and restart it, as
    /// `scripts\update.ps1` on the host would. Only honoured when the host
    /// user allowed it ([`HostControl::RemoteUpdate`]); the browser chooses
    /// nothing about what is built. Answered with
    /// [`HostControl::UpdateStatus`] or [`HostControl::Error`].
    UpdateHost,
}

/// Messages on the reliable `control` data channel, host -> browser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum HostControl {
    Displays {
        displays: Vec<DisplayInfo>,
        active: Option<DisplayId>,
    },
    DisplayChanged {
        id: DisplayId,
        config: VideoConfig,
    },
    Pong {
        client_time_ms: f64,
    },
    /// A file the host user chose to share; the client may request it by id.
    /// `name` is a bare file name, for display and as the suggested name.
    FileOffer {
        id: FileId,
        name: String,
        size: u64,
    },
    /// The host user stopped sharing the file: it can't be requested, and a
    /// transfer of it stopped.
    FileWithdrawn {
        id: FileId,
    },
    /// Sending a requested file failed (it couldn't be read, or the id is
    /// not on offer). It may be requested again if still on offer.
    FileFailed {
        id: FileId,
        message: String,
    },
    /// Whether the host accepts files from the browser, and how big. Sent
    /// when the `control` channel opens.
    Uploads {
        enabled: bool,
        max_size: u64,
    },
    /// Send the bytes of [`ClientControl::UploadFile`] `id`.
    UploadAccepted {
        id: FileId,
    },
    /// The host has written the first `received` bytes of the upload.
    UploadAck {
        id: FileId,
        received: u64,
    },
    /// The whole file is on the host, saved as `name` in its folder for
    /// received files.
    UploadSaved {
        id: FileId,
        name: String,
    },
    /// The host refused the file or stopped receiving it, and deleted what
    /// it had.
    UploadFailed {
        id: FileId,
        message: String,
    },
    /// Whether this browser may update and restart the host
    /// ([`ClientControl::UpdateHost`]). Sent when the `control` channel opens.
    RemoteUpdate {
        enabled: bool,
    },
    /// How an update this browser started is going. `Started` is followed
    /// by the host restarting (the session drops and the browser reconnects
    /// to the new build) unless `Finished` (nothing to update) or `Failed`
    /// comes first.
    UpdateStatus {
        state: UpdateState,
        message: String,
    },
    /// Gamepad rumble, motor strengths 0.0..=1.0.
    Rumble {
        index: u8,
        strong: f32,
        weak: f32,
    },
    /// Sent about once a second while video runs, for the browser's stats
    /// overlay and auto-tune.
    VideoStats(VideoStats),
    /// The host pointer changed shape, or was hidden or shown. Sent when the
    /// stream starts and on every change.
    CursorShape(CursorShape),
    /// Where the host pointer's hotspot is, in desktop pixels from the
    /// streamed display's top-left corner (outside the display when the
    /// pointer is on another one). Sent on change while the pointer shows and
    /// the browser asked with [`ClientControl::SetCursorPositions`]; only the
    /// newest matters.
    CursorPosition {
        x: i32,
        y: i32,
    },
    /// The mic the browser turned on can't play on the host (`--no-mic`, or
    /// no such device). The browser should stop sending it.
    MicUnavailable {
        message: String,
    },
    Error {
        message: String,
    },
    /// The session is over, as signaling's `bye`: sent here once the browser
    /// has handed signaling off ([`crate::ClientSignal::Handoff`]), just
    /// before the host closes the peer connection.
    Bye {
        reason: ByeReason,
    },
}

/// See [`HostControl::UpdateStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UpdateState {
    /// The build runs; the host restarts when it succeeds.
    Started,
    /// The script ended without restarting the host, e.g. already up to date.
    Finished,
    /// The script failed; the old build keeps running.
    Failed,
}

/// The host's side of the stream over the last stats interval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoStats {
    /// Bitrate the encoder is set to now.
    pub encoder_kbps: u32,
    /// The browser's bitrate setting: the most the encoder may use.
    pub max_kbps: u32,
    /// What the network is estimated to carry, if known yet.
    pub bwe_kbps: Option<u32>,
    /// Encoded frames handed to the network per second.
    pub fps: f32,
    /// Bitrate of those frames.
    pub sent_kbps: u32,
    /// Average capture-to-encoded time.
    pub encode_ms: f32,
    /// Keyframes asked for (by the browser or after a dropped frame) in
    /// the interval: each is a stall or a soft picture, so fewer is better.
    pub keyframe_requests: u32,
    /// Fraction of packets the browser reports lost, 0..=1.
    pub loss: Option<f32>,
    /// Of the keyframe requests, those from the browser: it lost a packet
    /// that no resend repaired.
    #[serde(default)]
    pub browser_keyframe_requests: u32,
    /// Frames dropped because the network send queue was too far behind.
    #[serde(default)]
    pub late_frames: u32,
    /// Frames dropped for another reason (the encoder got too far ahead, or
    /// they built on a dropped frame).
    #[serde(default)]
    pub lost_frames: u32,
    /// Packets the host's socket refused because its buffer was full.
    #[serde(default)]
    pub send_drops: u32,
    /// Late or dropped frames the encoder skipped over without a keyframe.
    #[serde(default)]
    pub recovered_frames: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_attention_shape() {
        let msg: ClientControl = serde_json::from_str(r#"{"type":"secureAttention"}"#).unwrap();
        assert_eq!(msg, ClientControl::SecureAttention);
    }

    #[test]
    fn update_messages_shape() {
        let msg: ClientControl = serde_json::from_str(r#"{"type":"updateHost"}"#).unwrap();
        assert_eq!(msg, ClientControl::UpdateHost);
        assert_eq!(
            serde_json::to_string(&HostControl::RemoteUpdate { enabled: true }).unwrap(),
            r#"{"type":"remoteUpdate","enabled":true}"#
        );
        assert_eq!(
            serde_json::to_string(&HostControl::UpdateStatus {
                state: UpdateState::Started,
                message: "m".into()
            })
            .unwrap(),
            r#"{"type":"updateStatus","state":"started","message":"m"}"#
        );
    }

    #[test]
    fn file_request_is_by_id() {
        let json = r#"{"type":"requestFile","id":7}"#;
        let msg: ClientControl = serde_json::from_str(json).unwrap();
        assert_eq!(msg, ClientControl::RequestFile { id: FileId(7) });
        assert!(
            serde_json::from_str::<ClientControl>(r#"{"type":"requestFile","path":"C:/x"}"#)
                .is_err()
        );
    }

    #[test]
    fn file_messages_shape() {
        let ack: ClientControl =
            serde_json::from_str(r#"{"type":"fileAck","id":7,"received":262144}"#).unwrap();
        assert_eq!(
            ack,
            ClientControl::FileAck {
                id: FileId(7),
                received: 262_144
            }
        );
        let cancel: ClientControl =
            serde_json::from_str(r#"{"type":"cancelFile","id":7}"#).unwrap();
        assert_eq!(cancel, ClientControl::CancelFile { id: FileId(7) });
        assert_eq!(
            serde_json::to_string(&HostControl::FileFailed {
                id: FileId(7),
                message: "gone".into()
            })
            .unwrap(),
            r#"{"type":"fileFailed","id":7,"message":"gone"}"#
        );
        assert_eq!(
            serde_json::to_string(&HostControl::Bye {
                reason: ByeReason::HostShutdown
            })
            .unwrap(),
            r#"{"type":"bye","reason":"host_shutdown"}"#
        );
        assert_eq!(
            serde_json::to_string(&HostControl::FileWithdrawn { id: FileId(7) }).unwrap(),
            r#"{"type":"fileWithdrawn","id":7}"#
        );
    }

    #[test]
    fn upload_messages_shape() {
        let up: ClientControl =
            serde_json::from_str(r#"{"type":"uploadFile","id":1,"name":"a.txt","size":5}"#)
                .unwrap();
        assert_eq!(
            up,
            ClientControl::UploadFile {
                id: FileId(1),
                name: "a.txt".into(),
                size: 5
            }
        );
        assert_eq!(
            serde_json::to_string(&HostControl::Uploads {
                enabled: true,
                max_size: 10
            })
            .unwrap(),
            r#"{"type":"uploads","enabled":true,"maxSize":10}"#
        );
        assert_eq!(
            serde_json::to_string(&HostControl::UploadAck {
                id: FileId(1),
                received: 5
            })
            .unwrap(),
            r#"{"type":"uploadAck","id":1,"received":5}"#
        );
    }

    #[test]
    fn file_chunk_round_trip() {
        let msg = file_chunk(FileId(FileId::MAX), 1 << 40, b"abc");
        assert_eq!(msg.len(), FILE_CHUNK_HEADER + 3);
        assert_eq!(
            parse_file_chunk(&msg),
            Some((FileId(FileId::MAX), 1 << 40, &b"abc"[..]))
        );
        assert_eq!(parse_file_chunk(&msg[..15]), None);
        assert_eq!(
            parse_file_chunk(&msg[..16]),
            Some((FileId(FileId::MAX), 1 << 40, &b""[..]))
        );
    }

    #[test]
    fn rumble_shape() {
        let msg = HostControl::Rumble {
            index: 0,
            strong: 1.0,
            weak: 0.5,
        };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"rumble","index":0,"strong":1.0,"weak":0.5}"#
        );
    }
}
