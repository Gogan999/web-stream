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

  const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required', '--disable-background-timer-throttling', '--disable-renderer-backgrounding', '--disable-backgrounding-occluded-windows'] });
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

  // Low-latency playout on every receiver; audio is sendrecv so the mic needs no renegotiation.
  const pcInfo = await client.evaluate(() => {
    const pc = window.__pcs.at(-1);
    return {
      jbt: pc.getReceivers().map((r) => r.jitterBufferTarget),
      audioDir: pc.getTransceivers().find((t) => t.receiver.track.kind === 'audio').direction,
    };
  });
  assert.deepEqual(pcInfo.jbt, [0, 0]);
  assert.equal(pcInfo.audioDir, 'sendrecv');

  // Browser-created channels with the agreed reliability.
  await host.waitForFunction(() => window.mockLog.filter((e) => e.ev === 'channel').length === 4);
  const channels = Object.fromEntries((await hostLog()).filter((e) => e.ev === 'channel').map((e) => [e.label, e]));
  assert.deepEqual(Object.keys(channels).sort(), ['control', 'file', 'input', 'input-rt']);
  assert.equal(channels['input-rt'].ordered, false);
  assert.equal(channels['input-rt'].maxRetransmits, 0);
  assert.equal(channels.input.ordered, true);
  assert.equal(channels.input.maxRetransmits, null);

  // Display list arrives and populates the picker.
  await client.waitForFunction(() => document.getElementById('display').options.length === 2);

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
  assert.equal(abs.ch, 'input-rt');
  assert.ok(inputs.some((e) => e.type === 'mouse-button' && e.button === 0 && e.down && e.ch === 'input'));
  assert.ok(inputs.some((e) => e.type === 'key' && e.usage === 0x1a && e.down));
  assert.ok(inputs.some((e) => e.type === 'key' && e.usage === 0x1a && !e.down));
  assert.ok(inputs.some((e) => e.type === 'wheel' && e.dy === -120));

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
  console.log(await client.textContent('#stats'));

  // Reload: the pairing cookie persists, so it connects straight away.
  await client.reload();
  await client.waitForFunction(() => document.getElementById('status').dataset.state === 'connected', null, { timeout: 15000 });
  assert.equal(await client.isVisible('#pair'), false);

  assert.deepEqual(errors, []);
});
