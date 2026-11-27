//! What a hardware encoder hands back for each frame.

use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::CaptureError;

/// One frame out of the encoder, with what the capture thread tagged it with when it went in.
pub(crate) struct Encoded<M> {
    pub data: Bytes,
    pub keyframe: bool,
    pub meta: M,
    /// From going in to the bitstream being ready.
    pub encode_time: Duration,
    /// Where the time in [`Self::encode_time`] and before it went.
    pub timing: EncodeTiming,
}

/// The instants one frame passed through the encoder, kept as plain values and only reported later.
#[derive(Clone, Copy)]
pub(crate) struct EncodeTiming {
    /// Just before a slot was asked for.
    pub before_slot: Instant,
    /// A slot was in hand.
    pub slot_taken: Instant,
    /// The encode call returned: the frame is in the encoder.
    pub submitted: Instant,
    /// The output thread got to the frame, before it waited for completion.
    pub wait_at: Instant,
    /// The wait for the completion event returned, before the bitstream was touched.
    pub completed: Instant,
    /// The bitstream was copied out and unlocked.
    pub done: Instant,
    /// How many times the lock answered `NV_ENC_ERR_LOCK_BUSY` and was tried again.
    pub lock_retries: u32,
}

/// What the output thread hands on for each frame.
pub(crate) type Finished<M> = std::result::Result<Encoded<M>, (M, CaptureError)>;
