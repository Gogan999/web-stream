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
    const OrigWs = window.WebSocket;
    window.__sockets = [];
    window.WebSocket = function (...a) { const ws = new OrigWs(...a); window.__sockets.push(ws); return ws; };
    Object.assign(window.WebSocket, { CONNECTING: 0, OPEN: 1, CLOSING: 2, CLOSED: 3, prototype: OrigWs.prototype });
    // No save dialog in a headless test: files take the in-memory path to downloads.
    window.showSaveFilePicker = undefined;
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

  // Once connected with control open, signaling is handed off and the WebSocket closes; everything
  // below runs over the peer connection alone.
  await host.waitForFunction(() => window.mockLog.some((e) => e.ev === 'handoff'));
  await client.waitForFunction(() => window.__sockets.at(-1)?.readyState === 3, null, { timeout: 5000 });
  assert.equal(await client.evaluate(() => document.getElementById('status').dataset.state), 'connected');
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
  // Settings live in one dropdown; a bitrate pick goes straight to the host.
  assert.equal(await client.isVisible('#bitrate'), false);
  await client.click('#more-toggle');
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
  // Audio quality: the host's own setting at first, then what is picked.
  assert.ok((await hostLog()).some((e) => e.type === 'setAudioConfig' && e.bitrateKbps === null && e.frameMs === null));
  await client.selectOption('#audio-kbps', '256');
  await client.selectOption('#audio-frame', '20');
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'setAudioConfig' && e.bitrateKbps === 256 && e.frameMs === 20));

  // A host that refuses the mic stops it: the box unticks and nothing is sent.
  await host.evaluate(() => { window.mockNoMic = true; });
  // click, not check: the refusal can untick the box before check() looks at it.
  await client.click('#mic');
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
  // Clicking the stream closed the settings dropdown; the mouse mode is in it.
  const mouseMode = async (mode) => {
    await client.click('#more-toggle');
    await client.selectOption('#mouse-mode', mode);
    await client.keyboard.press('Escape');
    await client.waitForSelector('#more', { state: 'hidden' });
  };
  await mouseMode('relative');
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
  await mouseMode('absolute');
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

  // Ctrl+Alt+Shift+E sends an Esc press and release (the browser keeps a real Esc in a window),
  // with the hotkey's modifiers let go first. The pointer is on the stream, so they reach the host.
  await client.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  const before = (await hostLog()).length;
  await client.keyboard.down('Control');
  await client.keyboard.down('Alt');
  await client.keyboard.down('Shift');
  await client.keyboard.press('KeyE');
  await client.keyboard.up('Shift');
  await client.keyboard.up('Alt');
  await client.keyboard.up('Control');
  await host.waitForFunction((n) => window.mockLog.slice(n).filter((e) => e.type === 'key' && e.usage === 0x29).length >= 2, before);
  await host.waitForFunction((n) => window.mockLog.slice(n).filter((e) => e.type === 'key' && e.usage === 0xe0 && !e.down).length >= 2, before);
  const keys = (await hostLog()).slice(before).filter((e) => e.type === 'key').map((e) => [e.usage, e.down]);
  assert.ok(!keys.some(([u]) => u === 0x08), 'E must not reach the host');
  const at = keys.findIndex(([u, d]) => u === 0x29 && d);
  const held = new Set();
  for (const [u, d] of keys.slice(0, at)) d ? held.add(u) : held.delete(u);
  assert.equal(held.size, 0, `modifiers up before Esc: ${JSON.stringify(keys)}`);
  assert.deepEqual(keys[at + 1], [0x29, false]);

  // Clicking the stream closed the dropdown. Stats overlay fills in.
  assert.equal(await client.isVisible('#more'), false);
  await client.click('#more-toggle');
  await client.selectOption('#stats-level', 'detailed');
  await client.click('#stats-toggle');
  await client.keyboard.press('Escape');
  assert.equal(await client.isVisible('#more'), false);
  await client.waitForFunction(() => /fps \d+/.test(document.getElementById('stats').textContent), null, { timeout: 5000 });
  await client.waitForFunction(() => /host enc 30\.0\/30\.0 Mb\/s/.test(document.getElementById('stats').textContent), null, { timeout: 5000 });
  console.log(await client.textContent('#stats'));

  // A shared file: the Files button appears with a count, Save downloads it, and the status says so.
  assert.equal(await client.isVisible('#files-toggle'), false);
  const text = 'hello from the host\n'.repeat(2000); // 40 KB: three chunks
  await host.evaluate((t) => window.mockShareFile(4242, 'notes.txt', t), text);
  await client.waitForSelector('#files-toggle:has-text("Files (1)")');
  await client.waitForFunction(() => /Host shared notes\.txt/.test(document.getElementById('status').textContent));
  await client.click('#files-toggle');
  await client.waitForSelector('#file-list li:has-text("notes.txt")');
  const [download] = await Promise.all([client.waitForEvent('download'), client.click('#file-list button:has-text("Save")')]);
  assert.equal(download.suggestedFilename(), 'notes.txt');
  const { readFile } = await import('node:fs/promises');
  assert.equal(await readFile(await download.path(), 'utf8'), text);
  await client.waitForSelector('#file-list li:has-text("Saved")');
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'fileAck' && e.id === 4242 && e.received === 40000));
  // The host stops sharing it: the row and the button go.
  await sendHost({ type: 'fileWithdrawn', id: 4242 });
  await client.waitForSelector('#files-toggle', { state: 'hidden' });

  // A host that takes files: Send files… sends one, and the row says where it went.
  await sendHost({ type: 'uploads', enabled: true, maxSize: 1 << 30 });
  await client.waitForSelector('#files-toggle:has-text("Files")');
  await client.click('#files-toggle');
  const upText = 'from the browser\n'.repeat(3000); // 51 KB: four chunks
  await client.setInputFiles('#upload-input', { name: 'up.txt', mimeType: 'text/plain', buffer: Buffer.from(upText) });
  await host.waitForFunction(() => window.mockUploaded['up.txt'], null, { timeout: 10000 });
  assert.equal(await host.evaluate(() => window.mockUploaded['up.txt']), upText);
  await client.waitForSelector('#upload-list li:has-text("On the host as up.txt")');
  await client.keyboard.press('Escape');

  // Update host: off until the host allows it, asks first, then waits for the restart.
  await client.click('#more-toggle');
  assert.equal(await client.isDisabled('#update-host'), true);
  await sendHost({ type: 'remoteUpdate', enabled: true });
  await client.waitForSelector('#update-host:not([disabled])');
  client.once('dialog', (d) => d.dismiss());
  await client.click('#update-host');
  client.once('dialog', (d) => d.accept());
  await client.click('#update-host');
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'updateHost'));
  assert.equal((await hostLog()).filter((e) => e.type === 'updateHost').length, 1);
  await sendHost({ type: 'updateStatus', state: 'started', message: 'Building the latest main' });
  await client.waitForSelector('#update-host:has-text("Updating host")');
  assert.equal(await client.isDisabled('#update-host'), true);
  await sendHost({ type: 'updateStatus', state: 'finished', message: 'Already up to date (abcd1234).' });
  await client.waitForFunction(() => /Host update: Already up to date/.test(document.getElementById('status').textContent));
  assert.equal(await client.isDisabled('#update-host'), false);
  await client.keyboard.press('Escape');

  // With no socket, the host's bye comes over control.
  await sendHost({ type: 'bye', reason: 'host_shutdown' });
  await client.waitForFunction(() => /host shut down/.test(document.getElementById('status').textContent));

  // Reload: the pairing cookie persists, so it connects straight away.
  await client.reload();
  await client.waitForFunction(() => document.getElementById('status').dataset.state === 'connected', null, { timeout: 15000 });
  assert.equal(await client.isVisible('#pair'), false);

  assert.deepEqual(errors, []);
});
