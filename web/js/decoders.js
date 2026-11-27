// What this browser decodes of the codecs the host encodes, and how well at the stream's size,
// for the host's automatic codec choice (the `decoders` control message).
import { decodableCodecs } from './settings.js';

// MediaCapabilities content types: RTP names for `webrtc`, MP4 codec strings for the other types.
const RTP_TYPES = Object.freeze({
  h264: 'video/H264;level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f',
  h265: 'video/H265',
  av1: 'video/AV1',
});
const FILE_TYPES = Object.freeze({
  h264: 'video/mp4; codecs="avc1.640033"',
  h265: 'video/mp4; codecs="hvc1.1.6.L153.B0"',
  av1: 'video/mp4; codecs="av01.0.13M.08"',
});

/** The first decodingInfo answer that says supported: webrtc, then media-source, then file. */
async function decodingInfo(mc, codec, { width, height, fps, bitrateKbps }) {
  const video = { width, height, framerate: fps, bitrate: bitrateKbps * 1000 };
  const tries = [
    ['webrtc', RTP_TYPES[codec]],
    ['media-source', FILE_TYPES[codec]],
    ['file', FILE_TYPES[codec]],
  ];
  for (const [type, contentType] of tries) {
    try {
      const info = await mc.decodingInfo({ type, video: { ...video, contentType } });
      if (info?.supported) return info;
    } catch { }
  }
  return null;
}

/** [{codec, smooth, powerEfficient}] for each codec RTCRtpReceiver lists, at `stream` size and rate. */
export async function detectDecoders(stream, {
  receiver = globalThis.RTCRtpReceiver,
  mediaCapabilities = globalThis.navigator?.mediaCapabilities,
} = {}) {
  const canAsk = typeof mediaCapabilities?.decodingInfo === 'function';
  return Promise.all(decodableCodecs(receiver).map(async (codec) => {
    const info = canAsk ? await decodingInfo(mediaCapabilities, codec, stream) : null;
    return { codec, smooth: info?.smooth === true, powerEfficient: info?.powerEfficient === true };
  }));
}
