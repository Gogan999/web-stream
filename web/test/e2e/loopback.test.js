// End-to-end: client page <-> dev mock host page over real WebRTC in headless Chromium.
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright';

const port = 18000 + Math.floor(Math.random() * 1000);
const base = `http://127.0.0.1:${port}`;

test('client streams from mock host, sends input and switches display', { timeout: 60000 }, async (t) => {
  const server = spawn(process.execPath, [fileURLToPath(new URL('../../dev/server.mjs', import.meta.url)), String(port)], { stdio: ['ignore', 'pipe', 'inherit'] });
  t.after(() => server.kill());
  await new Promise((r) => server.stdout.once('data', r));

  const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required', '--disable-background-timer-throttling', '--disable-renderer-backgrounding', '--disable-backgrounding-occluded-windows', '--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream'] });
  t.after(() => browser.close());
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });

  const host = await ctx.newPage();
  const errors = [];
  host.on('pageerror', (e) => errors.push('host: ' + e.message));
  await host.goto(`${base}/dev/mock-host.html`);
  await host.waitForFunction(() => window.mockLog?.some((e) => e.ev === 'signaling-open'));

  const client = await ctx.newPage();
  client.on('pageerror', (e) => errors.push('client: ' + e.message));
  // Keep a handle on the client's peer connections so the test can inspect them.
  await client.addInitScript(() => {
    const Orig = window.RTCPeerConnection;
    window.__pcs = [];
    window.RTCPeerConnection = function (...a) { const pc = new Orig(...a); window.__pcs.push(pc); return pc; };
    window.RTCPeerConnection.prototype = Orig.prototype;
  });
  await client.goto(`${base}/?autoconnect`);

  // Unpaired: pairing screen, wrong code rejected, right code pairs and connects.
  await client.waitForSelector('#pair:not([hidden])');
  assert.equal(await client.isVisible('#video'), false);
  await client.click('#pair-start');
  await host.waitForFunction(() => window.mockPairingCode);
  const code = await host.evaluate(() => window.mockPairingCode);
  await client.fill('#pair-code', code === '000000' ? '111111' : '000000');
  await client.click('#pair-confirm');
  await client.waitForFunction(() => /Wrong code, 2 attempts left/.test(document.getElementById('pair-msg').textContent));
  await client.fill('#pair-code', code);
  await client.click('#pair-confirm');
  await client.waitForFunction(() => document.getElementById('status').dataset.state === 'connected', null, { timeout: 15000 });
  await client.waitForFunction(() => document.getElementById('video').videoWidth === 1280, null, { timeout: 15000 });

  const hostLog = () => host.evaluate(() => window.mockLog);

  // Low-latency playout on every receiver; audio is answered sendrecv so the mic can be added later.
  const pcInfo = await client.evaluate(() => {
    const pc = window.__pcs.at(-1);
    return {
      jbt: Object.fromEntries(pc.getReceivers().map((r) => [r.track.kind, r.jitterBufferTarget])),
      stereo: /a=fmtp:\d+ .*stereo=1/.test(pc.localDescription.sdp),
      audioDir: pc.getTransceivers().find((t) => t.receiver.track.kind === 'audio').direction,
    };
  });
  // Video plays with no jitter buffer; audio keeps Chrome's adaptive one, and asks for stereo.
  assert.equal(pcInfo.jbt.video, 0);
  assert.equal(pcInfo.jbt.audio, null);
  assert.ok(pcInfo.stereo);
  assert.equal(pcInfo.audioDir, 'sendrecv');

  // Host-opened channels with the agreed reliability.
  await host.waitForFunction(() => window.mockLog.filter((e) => e.ev === 'channel').length === 4);
  const channels = Object.fromEntries((await hostLog()).filter((e) => e.ev === 'channel').map((e) => [e.label, e]));
  assert.deepEqual(Object.keys(channels).sort(), ['control', 'file', 'input', 'input-motion']);
  assert.equal(channels['input-motion'].ordered, false);
  assert.equal(channels['input-motion'].maxRetransmits, 0);
  assert.equal(channels.input.ordered, true);
  assert.equal(channels.input.maxRetransmits, null);

  // Display list arrives and populates the picker.
  await client.waitForFunction(() => document.getElementById('display').options.length === 2);

  // Once the host says what it streams, the client asks for its settings: the screen size fitted
  // to the 1280x720 display, at the matching auto bitrate.
  await host.waitForFunction(() => window.mockLog.some((e) => e.ev === 'control' && e.type === 'setVideoConfig'));
  const asked = (await hostLog()).find((e) => e.type === 'setVideoConfig').config;
  assert.deepEqual(
    [asked.width, asked.height, asked.fps, asked.quality, asked.codec],
    [1280, 720, 60, 'balanced', 'h264'],
  );
  assert.ok(asked.bitrateKbps > 5000 && asked.bitrateKbps < 20000, String(asked.bitrateKbps));
  // A bitrate pick goes straight to the host.
  await client.selectOption('#bitrate', '30000');
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'setVideoConfig' && e.config.bitrateKbps === 30000));
  // Matching the host resolution asks the host, which refuses unless allowed; the refusal shows.
  await client.check('#match-host');
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'setHostResolution'));
  await client.waitForFunction(() => /allow-resolution-change/.test(document.getElementById('status').textContent));
  await client.uncheck('#match-host');

  // Mic: off by default (the host is told so), on sends audio and tells the host.
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'setMicEnabled' && e.enabled === false));
  assert.equal(await host.evaluate(() => window.mockMicPackets()), 0);
  await client.check('#mic');
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'setMicEnabled' && e.enabled === true));
  await host.waitForFunction(async () => (await window.mockMicPackets()) > 10, null, { timeout: 10000, polling: 250 });
  await client.uncheck('#mic');
  await host.waitForFunction(() => window.mockLog.filter((e) => e.type === 'setMicEnabled' && e.enabled === false).length === 2);
  assert.equal(await client.evaluate(() => window.__pcs.at(-1).getSenders().find((x) => x.track)?.track ?? null), null);
  // A host that refuses the mic stops it: the box unticks and nothing is sent.
  await host.evaluate(() => { window.mockNoMic = true; });
  await client.check('#mic');
  await client.waitForFunction(() => !document.getElementById('mic').checked && /--no-mic/.test(document.getElementById('status').textContent));
  await client.waitForFunction(() => !window.__pcs.at(-1).getSenders().some((x) => x.track?.kind === 'audio'));
  await host.evaluate(() => { window.mockNoMic = false; });

  // Absolute mouse + keyboard.
  await client.selectOption('#mouse-mode', 'absolute');
  const box = await client.locator('#video').boundingBox();
  await client.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await client.mouse.down();
  await client.mouse.up();
  await client.keyboard.press('KeyW');
  await client.mouse.wheel(0, 100);
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'wheel'));
  const inputs = (await hostLog()).filter((e) => e.ev === 'input');
  const abs = inputs.find((e) => e.type === 'mouse-abs');
  assert.ok(abs && Math.abs(abs.x - 32768) < 700 && Math.abs(abs.y - 32768) < 700, JSON.stringify(abs));
  assert.equal(abs.ch, 'input-motion');
  assert.ok(inputs.some((e) => e.type === 'mouse-button' && e.button === 0 && e.down && e.ch === 'input'));
  assert.ok(inputs.some((e) => e.type === 'key' && e.usage === 0x1a && e.down));
  assert.ok(inputs.some((e) => e.type === 'key' && e.usage === 0x1a && !e.down));
  assert.ok(inputs.some((e) => e.type === 'wheel' && e.dy === -120));

  // Host pointer: absolute mode shows it as the video's CSS cursor, relative mode as an overlay.
  const sendHost = (msg) => host.evaluate((m) => window.__control.send(JSON.stringify(m)), msg);
  const px = (rgba, n) => Buffer.from(Array(n).fill(rgba).flat()).toString('base64');
  const cursorStyle = () => client.evaluate(() => document.getElementById('video').style.cursor);
  await sendHost({ type: 'cursorShape', kind: 'ew-resize', id: 1, image: { width: 4, height: 4, hotX: 2, hotY: 2, rgba: px([0, 0, 0, 255], 16) } });
  await sendHost({ type: 'cursorPosition', x: 640, y: 360 });
  await client.waitForFunction(() => document.getElementById('video').style.cursor === 'ew-resize');
  await sendHost({ type: 'cursorShape', kind: 'custom', id: 2, image: { width: 2, height: 2, hotX: 1, hotY: 0, rgba: px([255, 0, 0, 255], 4) } });
  await client.waitForFunction(() => document.getElementById('video').style.cursor.startsWith('url("data:image/png'));
  assert.match(await cursorStyle(), / 1 0, default$/);
  assert.equal(await client.isVisible('#cursor'), false, 'no overlay in absolute mode');
  await client.selectOption('#mouse-mode', 'relative');
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'setCursorPositions' && e.enabled));
  await sendHost({ type: 'cursorShape', kind: 'ew-resize', id: 1 }); // image already sent: id only
  await client.waitForSelector('#cursor', { state: 'visible' });
  assert.equal(await cursorStyle(), '');
  const overlay = await client.locator('#cursor').boundingBox();
  // The hotspot (2,2 of a 4x4 image) sits on the middle of the picture.
  assert.ok(Math.abs(overlay.x + overlay.width / 2 - (box.x + box.width / 2)) < 2, JSON.stringify({ overlay, box }));
  assert.ok(Math.abs(overlay.y + overlay.height / 2 - (box.y + box.height / 2)) < 2, JSON.stringify({ overlay, box }));
  await sendHost({ type: 'cursorShape', kind: 'none' });
  await client.waitForSelector('#cursor', { state: 'hidden' });
  await client.selectOption('#mouse-mode', 'absolute');
  await client.waitForFunction(() => document.getElementById('video').style.cursor === 'none');

  // Tab never lands in the hidden on-screen keyboard textarea.
  await client.mouse.move(1, 1); // off the stream, so Tab moves focus instead of going to the host
  await client.focus('#connect');
  let tabbed = false;
  for (let i = 0; i < 20; i++) {
    await client.keyboard.press('Tab');
    tabbed ||= await client.evaluate(() => document.activeElement?.id === 'kbd-input');
  }
  assert.ok(!tabbed);

  // Hotkey switches display without forwarding the digit, and the stream changes resolution.
  await client.keyboard.down('Control');
  await client.keyboard.down('Alt');
  await client.keyboard.down('Shift');
  await client.keyboard.press('Digit2');
  await client.keyboard.up('Shift');
  await client.keyboard.up('Alt');
  await client.keyboard.up('Control');
  await client.waitForFunction(() => document.getElementById('video').videoWidth === 1920, null, { timeout: 15000 });
  const after = await hostLog();
  assert.ok(after.some((e) => e.ev === 'control' && e.type === 'selectDisplay' && e.id === 1));
  assert.ok(!after.some((e) => e.type === 'key' && e.usage === 0x1f), 'Digit2 must not reach the host');

  // Stats overlay fills in.
  await client.click('#stats-toggle');
  await client.waitForFunction(() => /fps \d+/.test(document.getElementById('stats').textContent), null, { timeout: 5000 });
  await client.waitForFunction(() => /host enc 30\.0\/30\.0 Mb\/s/.test(document.getElementById('stats').textContent), null, { timeout: 5000 });
  console.log(await client.textContent('#stats'));

  // Reload: the pairing cookie persists, so it connects straight away.
  await client.reload();
  await client.waitForFunction(() => document.getElementById('status').dataset.state === 'connected', null, { timeout: 15000 });
  assert.equal(await client.isVisible('#pair'), false);

  assert.deepEqual(errors, []);
});
