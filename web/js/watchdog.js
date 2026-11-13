// Notices a video stream that arrives but never decodes (a lost or unusable keyframe after the host
// restarted its video, a decoder that dropped out) and says when to ask the host for a new keyframe.
// Chrome's own keyframe requests (PLI) are all the browser had before, and they did not rescue a stream
// that stayed dead for minutes. Pure over plain numbers so it can be tested without a connection.

export const STALL_MS = 2000; // packets arriving with no new decoded frame for this long = stalled
export const RETRY_MS = 2000; // then ask again this often while it lasts

export class StallWatchdog {
  constructor({ stallMs = STALL_MS, retryMs = RETRY_MS } = {}) {
    this.stallMs = stallMs;
    this.retryMs = retryMs;
    this.reset();
  }

  reset() {
    this.last = null;
    this.since = null;
    this.asked = null;
  }

  // `video` is the receiver's inbound-rtp stats (or null); `now` in ms; `hidden` is a background tab,
  // where the browser throttles decoding and a stall is expected. True: ask for a keyframe now.
  update(video, now, hidden = false) {
    if (!video || hidden) {
      this.reset();
      return false;
    }
    const cur = { bytes: video.bytesReceived ?? 0, frames: video.framesDecoded ?? 0 };
    const prev = this.last;
    this.last = cur;
    if (!prev) return false;
    const arriving = cur.bytes > prev.bytes;
    const decoding = cur.frames > prev.frames;
    if (decoding || !arriving) {
      // Healthy, or nothing is being sent (not this watchdog's business).
      this.since = null;
      this.asked = null;
      return false;
    }
    this.since ??= now;
    if (now - this.since < this.stallMs) return false;
    if (this.asked != null && now - this.asked < this.retryMs) return false;
    this.asked = now;
    return true;
  }
}
