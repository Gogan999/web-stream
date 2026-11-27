import test from 'node:test';
import assert from 'node:assert/strict';
import { detectDecoders } from '../../js/decoders.js';

const stream = { width: 1920, height: 1080, fps: 60, bitrateKbps: 20000 };
const receiver = (mimes) => ({ getCapabilities: () => ({ codecs: mimes.map((mimeType) => ({ mimeType })) }) });

// MediaCapabilities stub: `answer(type, contentType)` returns the info, or throws for an unknown type.
function capabilities(answer) {
  const calls = [];
  return {
    calls,
    decodingInfo: async (config) => {
      calls.push(config);
      return answer(config.type, config.video.contentType);
    },
  };
}

test('webrtc decodingInfo rates each codec the receiver lists', async () => {
  const mc = capabilities((type, ct) => ({
    supported: true,
    smooth: true,
    powerEfficient: !ct.includes('AV1'),
  }));
  const got = await detectDecoders(stream, { receiver: receiver(['video/VP8', 'video/H264', 'video/H265', 'video/AV1']), mediaCapabilities: mc });
  assert.deepEqual(got, [
    { codec: 'h264', smooth: true, powerEfficient: true },
    { codec: 'h265', smooth: true, powerEfficient: true },
    { codec: 'av1', smooth: true, powerEfficient: false },
  ]);
  assert.ok(mc.calls.every((c) => c.type === 'webrtc'));
  assert.deepEqual(
    { ...mc.calls[0].video, contentType: undefined },
    { width: 1920, height: 1080, framerate: 60, bitrate: 20000000, contentType: undefined },
  );
  assert.match(mc.calls[0].video.contentType, /^video\/H264/);
});

test('without the webrtc type it falls back to media-source, then file', async () => {
  const mc = capabilities((type, ct) => {
    if (type === 'webrtc') throw new TypeError('unknown type');
    if (type === 'media-source' && ct.includes('hvc1')) return { supported: false, smooth: false, powerEfficient: false };
    return { supported: true, smooth: true, powerEfficient: type === 'file' };
  });
  const got = await detectDecoders(stream, { receiver: receiver(['video/H264', 'video/H265']), mediaCapabilities: mc });
  assert.deepEqual(got, [
    { codec: 'h264', smooth: true, powerEfficient: false },
    { codec: 'h265', smooth: true, powerEfficient: true },
  ]);
  assert.deepEqual(mc.calls.filter((c) => c.video.contentType.includes('hvc1')).map((c) => c.type), ['media-source', 'file']);
  assert.match(mc.calls.find((c) => c.type === 'media-source').video.contentType, /^video\/mp4; codecs="avc1\./);
});

test('no answer, or no MediaCapabilities: supported but neither smooth nor efficient', async () => {
  const none = capabilities(() => { throw new TypeError('nope'); });
  assert.deepEqual(await detectDecoders(stream, { receiver: receiver(['video/AV1']), mediaCapabilities: none }), [
    { codec: 'av1', smooth: false, powerEfficient: false },
  ]);
  assert.equal(none.calls.length, 3);
  assert.deepEqual(await detectDecoders(stream, { receiver: receiver(['video/H264']), mediaCapabilities: undefined }), [
    { codec: 'h264', smooth: false, powerEfficient: false },
  ]);
  assert.deepEqual(await detectDecoders(stream, { receiver: receiver(['video/H264']), mediaCapabilities: {} }), [
    { codec: 'h264', smooth: false, powerEfficient: false },
  ]);
});

test('without RTCRtpReceiver capabilities every codec is reported', async () => {
  const got = await detectDecoders(stream, { receiver: undefined, mediaCapabilities: undefined });
  assert.deepEqual(got.map((d) => d.codec), ['h264', 'h265', 'av1']);
});
