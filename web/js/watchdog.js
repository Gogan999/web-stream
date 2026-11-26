// Notices a video stream that arrives but never decodes and says when to ask the host for a new
// keyframe.

export const STALL_MS = 2000;
export const RETRY_MS = 2000;

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

  // `video` is the receiver's inbound-rtp stats (or null); `now` in ms; `hidden` is a background
  // tab.
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
