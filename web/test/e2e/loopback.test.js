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
  await client.addInitScript(() => {
    const Orig = window.RTCPeerConnection;
    window.__pcs = [];
    window.RTCPeerConnection = function (...a) { const pc = new Orig(...a); window.__pcs.push(pc); return pc; };
    window.RTCPeerConnection.prototype = Orig.prototype;
    const OrigWs = window.WebSocket;
    window.__sockets = [];
    window.WebSocket = function (...a) { const ws = new OrigWs(...a); window.__sockets.push(ws); return ws; };
    Object.assign(window.WebSocket, { CONNECTING: 0, OPEN: 1, CLOSING: 2, CLOSED: 3, prototype: OrigWs.prototype });
    window.showSaveFilePicker = undefined;
  });
  await client.goto(`${base}/?autoconnect`);

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

  // Low-latency playout on every receiver; audio is answered sendrecv.
  const pcInfo = await client.evaluate(() => {
    const pc = window.__pcs.at(-1);
    return {
      jbt: Object.fromEntries(pc.getReceivers().map((r) => [r.track.kind, r.jitterBufferTarget])),
      stereo: /a=fmtp:\d+ .*stereo=1/.test(pc.localDescription.sdp),
      audioDir: pc.getTransceivers().find((t) => t.receiver.track.kind === 'audio').direction,
    };
  });
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

  await host.waitForFunction(() => window.mockLog.some((e) => e.ev === 'handoff'));
  await client.waitForFunction(() => window.__sockets.at(-1)?.readyState === 3, null, { timeout: 5000 });
  assert.equal(await client.evaluate(() => document.getElementById('status').dataset.state), 'connected');
  assert.equal(channels.input.ordered, true);
  assert.equal(channels.input.maxRetransmits, null);

  await client.waitForFunction(() => document.getElementById('display').options.length === 2);
  assert.equal(await client.isVisible('#display-quick'), true);
  assert.equal(await client.locator('#display-quick option').count(), 2);
  assert.equal(await client.isVisible('#connect-big'), false);
  assert.equal(await client.textContent('#connect'), 'Disconnect');

  assert.equal(await client.textContent('#mouse-label'), 'Game');
  await client.click('#mouse-toggle');
  assert.equal(await client.textContent('#mouse-label'), 'Desktop');
  assert.equal(await client.inputValue('#mouse-mode'), 'absolute');
  await client.click('#mouse-toggle');
  assert.equal(await client.inputValue('#mouse-mode'), 'relative');
  assert.equal(await client.getAttribute('#mic-toggle', 'aria-pressed'), 'false');

  await host.waitForFunction(() => window.mockLog.some((e) => e.ev === 'control' && e.type === 'setVideoConfig'));
  const asked = (await hostLog()).find((e) => e.type === 'setVideoConfig').config;
  assert.deepEqual(
    [asked.width, asked.height, asked.fps, asked.quality],
    [1280, 720, 60, 'balanced'],
  );
  assert.ok(['h264', 'h265', 'av1'].includes(asked.codec), asked.codec);
  assert.ok(asked.bitrateKbps > 5000 && asked.bitrateKbps < 20000, String(asked.bitrateKbps));
  assert.equal(await client.isVisible('#bitrate'), false);
  await client.click('#more-toggle');
  assert.equal(await client.isVisible('#priority'), true);
  assert.equal(await client.isVisible('#resolution'), true);
  await client.click('#set-advanced > summary');
  await client.selectOption('#bitrate', '30000');
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'setVideoConfig' && e.config.bitrateKbps === 30000));

  // Decoders go to the host once control opens; Auto, the default, lets it pick and shows the pick.
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'decoders'));
  const reported = (await hostLog()).find((e) => e.type === 'decoders');
  const offered = await client.$$eval('#codec option', (os) => os.map((o) => o.value));
  assert.equal(reported.auto, true);
  assert.equal(offered[0], 'auto');
  assert.deepEqual(reported.decoders.map((d) => d.codec), offered.slice(1));
  assert.ok(reported.decoders.every((d) => typeof d.smooth === 'boolean' && typeof d.powerEfficient === 'boolean'));
  assert.equal(await client.inputValue('#codec'), 'auto');
  if (reported.decoders.length) {
    await client.waitForFunction(() => /^Auto \(.+, picked for this browser\)$/.test(document.querySelector('#codec option').textContent));
    // Choosing a codec turns Auto off first, then asks for it.
    const pick = offered.at(-1);
    const start = (await hostLog()).length;
    await client.selectOption('#codec', pick);
    await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'decoders' && e.auto === false));
    await client.waitForFunction(() => document.querySelector('#codec option').textContent === 'Auto');
    const after = (await hostLog()).slice(start);
    const off = after.findIndex((e) => e.type === 'decoders' && e.auto === false);
    const ask = after.findIndex((e) => e.type === 'setVideoConfig' && e.config.codec === pick);
    assert.ok(ask === -1 || ask > off, JSON.stringify(after));
    await client.selectOption('#codec', 'auto');
    await host.waitForFunction(() => window.mockLog.filter((e) => e.type === 'decoders' && e.auto === true).length === 2);
    await client.waitForFunction(() => /picked for this browser/.test(document.querySelector('#codec option').textContent));
  }
  await client.check('#match-host');
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'setHostResolution'));
  await client.waitForFunction(() => /allow-resolution-change/.test(document.getElementById('status').textContent));
  await client.uncheck('#match-host');

  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'setMicEnabled' && e.enabled === false));
  assert.equal(await host.evaluate(() => window.mockMicPackets()), 0);
  await client.check('#mic');
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'setMicEnabled' && e.enabled === true));
  await host.waitForFunction(async () => (await window.mockMicPackets()) > 10, null, { timeout: 10000, polling: 250 });
  assert.equal(await client.getAttribute('#mic-toggle', 'aria-pressed'), 'true');
  await client.uncheck('#mic');
  await host.waitForFunction(() => window.mockLog.filter((e) => e.type === 'setMicEnabled' && e.enabled === false).length === 2);
  assert.equal(await client.getAttribute('#mic-toggle', 'aria-pressed'), 'false');
  assert.equal(await client.evaluate(() => window.__pcs.at(-1).getSenders().find((x) => x.track)?.track ?? null), null);
  assert.ok((await hostLog()).some((e) => e.type === 'setAudioConfig' && e.bitrateKbps === null && e.frameMs === null));
  // Audio quality is under Advanced (opened above).
  assert.equal(await client.isVisible('#audio-kbps'), true);
  await client.selectOption('#audio-kbps', '256');
  await client.selectOption('#audio-frame', '20');
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'setAudioConfig' && e.bitrateKbps === 256 && e.frameMs === 20));

  await host.evaluate(() => { window.mockNoMic = true; });
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

  await ctx.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: base });
  await client.bringToFront();
  await client.waitForSelector('#clipboard', { state: 'visible' });
  await host.evaluate(() => window.__control.send(JSON.stringify({ type: 'clipboard', text: 'h\u00e9llo from the PC' })));
  await client.waitForFunction(async () => (await navigator.clipboard.readText()) === 'h\u00e9llo from the PC');
  await client.evaluate(() => navigator.clipboard.writeText('typed on the client \u4e16'));
  await client.waitForFunction(() => navigator.permissions.query({ name: 'clipboard-read' }).then((p) => p.state === 'granted'));
  await client.click('#video');
  const keyEvents = () => host.evaluate(() => window.mockLog.length);
  const clipFrom = await keyEvents();
  await client.keyboard.down('Control');
  await client.keyboard.press('KeyV');
  await client.keyboard.up('Control');
  await host.waitForFunction((n) => window.mockLog.slice(n).some((e) => e.ev === 'input' && e.type === 'key' && e.usage === 0x19 && e.down), clipFrom);
  const clipLog = (await hostLog()).slice(clipFrom);
  const setAt = clipLog.findIndex((e) => e.type === 'setClipboard');
  const vAt = clipLog.findIndex((e) => e.type === 'key' && e.usage === 0x19 && e.down);
  assert.ok(setAt >= 0 && setAt < vAt, 'the clipboard is set before V goes down: ' + JSON.stringify(clipLog.map((e) => e.type)));
  assert.equal(await host.evaluate(() => window.mockClipboard), 'typed on the client \u4e16');
  // The setting turns it all off.
  await client.click('#more-toggle');
  await client.uncheck('#clip-enabled');
  await client.waitForSelector('#clipboard', { state: 'hidden' });
  await client.check('#clip-enabled');
  await client.waitForSelector('#clipboard', { state: 'visible' });
  await client.click('#more-toggle');

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
  await sendHost({ type: 'cursorShape', kind: 'ew-resize', id: 1 });
  await client.waitForSelector('#cursor', { state: 'visible' });
  assert.equal(await cursorStyle(), '');
  const overlay = await client.locator('#cursor').boundingBox();
  assert.ok(Math.abs(overlay.x + overlay.width / 2 - (box.x + box.width / 2)) < 2, JSON.stringify({ overlay, box }));
  assert.ok(Math.abs(overlay.y + overlay.height / 2 - (box.y + box.height / 2)) < 2, JSON.stringify({ overlay, box }));
  await sendHost({ type: 'cursorShape', kind: 'none' });
  await client.waitForSelector('#cursor', { state: 'hidden' });
  await mouseMode('absolute');
  await client.waitForFunction(() => document.getElementById('video').style.cursor === 'none');

  // Open the settings dropdown unless it already is.
  const openSettings = async () => {
    for (let i = 0; i < 3 && !(await client.isVisible('#more')); i++) {
      await client.click('#more-toggle');
      await client.waitForTimeout(100);
    }
  };
  // Close it again if it is open.
  const closeSettings = async () => {
    for (let i = 0; i < 3 && (await client.isVisible('#more')); i++) {
      await client.click('#more-toggle');
      await client.waitForTimeout(100);
    }
    await client.waitForSelector('#more', { state: 'hidden' });
  };
  await openSettings();
  await client.selectOption('#display2', '1');
  await client.keyboard.press('Escape');
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'selectSecondDisplay' && e.id === 1));
  await client.waitForFunction(() => document.getElementById('video2').videoWidth === 1920, null, { timeout: 15000 });
  assert.ok(await client.evaluate(() => document.body.classList.contains('dual')));
  assert.equal(await client.evaluate(() => document.getElementById('video').videoWidth), 1280, 'the first video keeps its own stream');
  const box2 = await client.locator('#video2').boundingBox();
  const box1 = await client.locator('#video').boundingBox();
  assert.ok(box2.x >= box1.x + box1.width - 2, 'side by side');
  await client.mouse.move(box2.x + box2.width / 4, box2.y + box2.height / 2);
  await client.mouse.down();
  await client.mouse.up();
  await host.waitForFunction(() => window.mockLog.some((e) => e.ev === 'input' && e.type === 'mouse-abs' && e.lane === 1));
  const onSecond = (await hostLog()).filter((e) => e.type === 'mouse-abs' && e.lane === 1).at(-1);
  assert.ok(Math.abs(onSecond.x - 16384) < 1500 && Math.abs(onSecond.y - 32768) < 1500, JSON.stringify(onSecond));
  await client.mouse.move(box1.x + box1.width / 2, box1.y + box1.height / 2);
  await host.waitForFunction(() => window.mockLog.filter((e) => e.type === 'mouse-abs').at(-1)?.lane === undefined);
  await openSettings();
  const [popup] = await Promise.all([client.waitForEvent('popup'), client.click('#popout')]);
  await client.keyboard.press('Escape');
  await popup.waitForFunction(() => document.querySelector('video')?.videoWidth === 1920, null, { timeout: 15000 });
  assert.equal(await client.evaluate(() => document.body.classList.contains('dual')), false);
  assert.equal(await client.evaluate(() => document.getElementById('video2').hidden), true);
  const loggedBefore = (await hostLog()).length;
  await popup.mouse.move(300, 300);
  await popup.mouse.down();
  await popup.mouse.up();
  await popup.keyboard.press('KeyW');
  await host.waitForFunction((n) => window.mockLog.slice(n).some((e) => e.type === 'mouse-abs' && e.lane === 1)
    && window.mockLog.slice(n).some((e) => e.type === 'key' && e.usage === 0x1a), loggedBefore);
  assert.ok((await hostLog()).slice(loggedBefore).some((e) => e.type === 'mouse-button' && e.button === 0 && e.down));
  await popup.close();
  await client.waitForFunction(() => document.body.classList.contains('dual') && !document.getElementById('video2').hidden);
  await client.waitForFunction(() => document.getElementById('video2').videoWidth === 1920);
  await openSettings();
  await client.selectOption('#display2', 'off');
  await client.keyboard.press('Escape');
  await client.waitForFunction(() => !document.body.classList.contains('dual') && document.getElementById('video2').hidden);
  await closeSettings();

  await client.mouse.move(1, 1);
  await client.focus('#connect');
  let tabbed = false;
  for (let i = 0; i < 20; i++) {
    await client.keyboard.press('Tab');
    tabbed ||= await client.evaluate(() => document.activeElement?.id === 'kbd-input');
  }
  assert.ok(!tabbed);

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

  // Stats overlay fills in.
  await closeSettings();
  await client.click('#more-toggle');
  // Advanced stays unfolded from the audio step.
  await client.selectOption('#stats-level', 'detailed');
  await client.click('#stats-toggle');
  await client.keyboard.press('Escape');
  assert.equal(await client.isVisible('#more'), false);
  await client.waitForFunction(() => /fps \d+/.test(document.getElementById('stats').textContent), null, { timeout: 5000 });
  await client.waitForFunction(() => /host enc 30\.0\/30\.0 Mb\/s/.test(document.getElementById('stats').textContent), null, { timeout: 5000 });
  console.log(await client.textContent('#stats'));

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

  await sendHost({ type: 'uploads', enabled: true, maxSize: 1 << 30 });
  await client.waitForSelector('#files-toggle:has-text("Files")');
  await client.click('#files-toggle');
  const upText = 'from the browser\n'.repeat(3000); // 51 KB: four chunks
  await client.setInputFiles('#upload-input', { name: 'up.txt', mimeType: 'text/plain', buffer: Buffer.from(upText) });
  await host.waitForFunction(() => window.mockUploaded['up.txt'], null, { timeout: 10000 });
  assert.equal(await host.evaluate(() => window.mockUploaded['up.txt']), upText);
  await client.waitForSelector('#upload-list li:has-text("On the host as up.txt")');
  await client.keyboard.press('Escape');

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

  await sendHost({ type: 'bye', reason: 'host_shutdown' });
  await client.waitForFunction(() => /host shut down/.test(document.getElementById('status').textContent));

  await client.reload();
  await client.waitForFunction(() => document.getElementById('status').dataset.state === 'connected', null, { timeout: 15000 });
  assert.equal(await client.isVisible('#pair'), false);

  assert.deepEqual(errors, []);
});
