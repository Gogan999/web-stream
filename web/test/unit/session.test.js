import test from 'node:test';
import assert from 'node:assert/strict';
import { withStereoOpus, minimizePlayout } from '../../js/session.js';

const answer = [
  'v=0', 'm=audio 9 UDP/TLS/RTP/SAVPF 111 63', 'a=rtpmap:111 opus/48000/2',
  'a=fmtp:111 minptime=10;useinbandfec=1', 'a=rtpmap:63 red/48000/2', 'a=fmtp:63 111/111', '',
].join('\r\n');

test('answer asks for stereo opus', () => {
  const out = withStereoOpus(answer);
  assert.match(out, /a=fmtp:111 minptime=10;useinbandfec=1;stereo=1;sprop-stereo=1\r\n/);
  assert.match(out, /a=fmtp:63 111\/111\r\n/);
  assert.equal(withStereoOpus(out), out);
});

test('only video gets the zero jitter buffer', () => {
  const video = { track: { kind: 'video' } };
  const audio = { track: { kind: 'audio' } };
  minimizePlayout(video);
  minimizePlayout(audio);
  assert.equal(video.jitterBufferTarget, 0);
  assert.equal(audio.jitterBufferTarget, undefined);
});
