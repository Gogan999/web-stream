// End-to-end on an emulated phone: touch gestures, the on-screen keyboard and the folded toolbar.
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright';

const port = 19000 + Math.floor(Math.random() * 1000);
const base = `http://127.0.0.1:${port}`;

test('phone: touch input, on-screen keyboard and small-screen toolbar', { timeout: 60000 }, async (t) => {
  const server = spawn(process.execPath, [fileURLToPath(new URL('../../dev/server.mjs', import.meta.url)), String(port)], { stdio: ['ignore', 'pipe', 'inherit'] });
  t.after(() => server.kill());
  await new Promise((r) => server.stdout.once('data', r));

  const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required', '--disable-background-timer-throttling', '--disable-renderer-backgrounding', '--disable-backgrounding-occluded-windows'] });
  t.after(() => browser.close());
  const hostCtx = await browser.newContext();
  const host = await hostCtx.newPage();
  const errors = [];
  host.on('pageerror', (e) => errors.push('host: ' + e.message));
  await host.goto(`${base}/dev/mock-host.html`);
  await host.waitForFunction(() => window.mockLog?.some((e) => e.ev === 'signaling-open'));

  // A Pixel-sized phone held sideways.
  const ctx = await browser.newContext({ viewport: { width: 915, height: 412 }, deviceScaleFactor: 2.6, isMobile: true, hasTouch: true });
  const client = await ctx.newPage();
  client.on('pageerror', (e) => errors.push('client: ' + e.message));
  await client.goto(`${base}/`);
  await client.waitForSelector('#pair:not([hidden])');
  await client.tap('#pair-start');
  await host.waitForFunction(() => window.mockPairingCode);
  await client.fill('#pair-code', await host.evaluate(() => window.mockPairingCode));
  await client.tap('#pair-confirm');
  await client.waitForFunction(() => document.getElementById('status').dataset.state === 'connected', null, { timeout: 15000 });
  await client.waitForFunction(() => document.getElementById('video').videoWidth > 0, null, { timeout: 15000 });

  // Settings fold away behind one button; the overlay buttons show.
  assert.equal(await client.isVisible('#bitrate'), false);
  assert.equal(await client.isVisible('#touchbar'), true);
  await client.tap('#more-toggle');
  assert.equal(await client.isVisible('#bitrate'), true);
  await client.tap('#more-toggle');

  const inputs = async () => (await host.evaluate(() => window.mockLog)).filter((e) => e.ev === 'input');
  const clearLog = () => host.evaluate(() => { window.mockLog.length = 0; });
  const box = await client.locator('#video').boundingBox();
  const cx = box.x + box.width / 2;
  const cy = box.y + box.height / 2;
  const cdp = await ctx.newCDPSession(client);
  const touch = (type, points) => cdp.send('Input.dispatchTouchEvent', { type, touchPoints: points });

  // Trackpad (relative mode): a tap is a left click, a drag is relative motion with no click.
  await clearLog();
  await client.touchscreen.tap(cx, cy);
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'mouse-button' && !e.down));
  await client.waitForTimeout(500); // past the tap-and-drag window
  await touch('touchStart', [{ x: cx, y: cy }]);
  for (let i = 1; i <= 5; i++) await touch('touchMove', [{ x: cx + i * 10, y: cy }]);
  await touch('touchEnd', []);
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'mouse-rel'));
  let got = await inputs();
  assert.deepEqual(got.filter((e) => e.type === 'mouse-button').map((e) => [e.button, e.down]), [[0, true], [0, false]]);
  const dx = got.filter((e) => e.type === 'mouse-rel').reduce((s, e) => s + e.dx, 0);
  assert.ok(dx > 50, `relative motion ${dx}`); // 50 CSS px, scaled up to picture pixels
  assert.ok(!got.some((e) => e.type === 'mouse-abs'));

  // Tap, then touch again and drag: the left button is held for the drag, pressed after moving.
  await clearLog();
  await touch('touchStart', [{ x: cx, y: cy }]);
  await touch('touchEnd', []);
  await touch('touchStart', [{ x: cx, y: cy }]);
  for (let i = 1; i <= 5; i++) await touch('touchMove', [{ x: cx + i * 10, y: cy }]);
  await touch('touchEnd', []);
  await host.waitForFunction(() => window.mockLog.filter((e) => e.type === 'mouse-button').length >= 4);
  got = await inputs();
  const btn = (e) => e.type === 'mouse-button';
  assert.deepEqual(got.filter(btn).map((e) => [e.button, e.down]), [[0, true], [0, false], [0, true], [0, false]]);
  const press = got.filter(btn)[2];
  assert.ok(got.findIndex((e) => e.type === 'mouse-rel') < got.indexOf(press), 'moved before pressing');
  assert.ok(got.slice(got.indexOf(press)).some((e) => e.type === 'mouse-rel'), 'moved while pressed');

  // Two-finger drag scrolls.
  await clearLog();
  await touch('touchStart', [{ x: cx - 50, y: cy, id: 1 }, { x: cx + 50, y: cy, id: 2 }]);
  for (let i = 1; i <= 5; i++) await touch('touchMove', [{ x: cx - 50, y: cy + i * 10, id: 1 }, { x: cx + 50, y: cy + i * 10, id: 2 }]);
  await touch('touchEnd', []);
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'wheel'));
  got = await inputs();
  assert.ok(got.filter((e) => e.type === 'wheel').every((e) => e.dy > 0));
  assert.ok(!got.some((e) => e.type === 'mouse-button'));

  // The host pointer, drawn over the video in relative mode.
  const sendHost = (msg) => host.evaluate((m) => window.__control.send(JSON.stringify(m)), msg);
  const px = Buffer.from(Array(32 * 32).fill([255, 255, 255, 255]).flat()).toString('base64');
  await sendHost({ type: 'cursorShape', kind: 'custom', id: 1, image: { width: 32, height: 32, hotX: 0, hotY: 0, rgba: px } });
  await sendHost({ type: 'cursorPosition', x: 100, y: 100 });
  await client.waitForSelector('#cursor:not([hidden])');
  const pointer = await client.locator('#cursor').boundingBox();

  // Pinch out zooms the view (not the host) and shows Fit; Fit zooms back out.
  await clearLog();
  await touch('touchStart', [{ x: cx - 40, y: cy, id: 1 }, { x: cx + 40, y: cy, id: 2 }]);
  for (let i = 1; i <= 8; i++) await touch('touchMove', [{ x: cx - 40 - i * 10, y: cy, id: 1 }, { x: cx + 40 + i * 10, y: cy, id: 2 }]);
  await touch('touchEnd', []);
  const zoomed = await client.locator('#video').boundingBox();
  assert.ok(zoomed.width > box.width * 1.8, `zoomed width ${zoomed.width}`);
  assert.equal(await client.isVisible('#zoom-reset'), true);
  await client.waitForTimeout(200);
  assert.ok(!(await inputs()).some((e) => e.type === 'wheel' || e.type === 'mouse-button'));
  // The pointer keeps its size on screen.
  const zp = await client.locator('#cursor').boundingBox();
  assert.ok(Math.abs(zp.width - pointer.width) < 1, `pointer ${pointer.width} -> ${zp.width}`);
  // Zoomed in, a two-finger drag still scrolls; with Pan on it moves the view instead.
  const drag = async () => {
    await touch('touchStart', [{ x: cx - 50, y: cy, id: 1 }, { x: cx + 50, y: cy, id: 2 }]);
    for (let i = 1; i <= 5; i++) await touch('touchMove', [{ x: cx - 50, y: cy + i * 10, id: 1 }, { x: cx + 50, y: cy + i * 10, id: 2 }]);
    await touch('touchEnd', []);
  };
  await clearLog();
  await drag();
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'wheel'));
  assert.equal((await client.locator('#video').boundingBox()).y, zoomed.y, 'scrolling left the view alone');
  await client.tap('#zoom-pan');
  await clearLog();
  await drag();
  await client.waitForTimeout(200);
  assert.ok(!(await inputs()).some((e) => e.type === 'wheel'), 'Pan on: no scroll');
  assert.ok((await client.locator('#video').boundingBox()).y > zoomed.y + 20, 'Pan on: the view moved');
  await client.tap('#zoom-pan');
  await client.tap('#zoom-reset');
  assert.equal((await client.locator('#video').boundingBox()).width, box.width);
  assert.equal(await client.isVisible('#zoom-reset'), false);

  // Direct (absolute mode): a tap clicks where the finger is.
  await client.tap('#more-toggle');
  await client.selectOption('#mouse-mode', 'absolute');
  await client.tap('#more-toggle');
  await clearLog();
  await client.touchscreen.tap(cx, cy);
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'mouse-button' && !e.down));
  got = await inputs();
  const abs = got.find((e) => e.type === 'mouse-abs');
  assert.ok(abs && Math.abs(abs.x - 32768) < 700 && Math.abs(abs.y - 32768) < 700, JSON.stringify(abs));
  // The click position goes on the reliable channel, ahead of the button.
  assert.equal(abs.ch, 'input');
  assert.ok(got.indexOf(abs) < got.findIndex((e) => e.type === 'mouse-button'));

  // A real pinch: the first finger slides before the second lands. Still a zoom, no drag on the host.
  await clearLog();
  await touch('touchStart', [{ x: cx - 40, y: cy, id: 1 }]);
  await touch('touchMove', [{ x: cx - 60, y: cy, id: 1 }]);
  await touch('touchStart', [{ x: cx - 60, y: cy, id: 1 }, { x: cx + 40, y: cy, id: 2 }]);
  for (let i = 1; i <= 8; i++) await touch('touchMove', [{ x: cx - 60 - i * 10, y: cy, id: 1 }, { x: cx + 40 + i * 10, y: cy, id: 2 }]);
  await touch('touchEnd', []);
  await client.waitForTimeout(200);
  assert.ok(!(await inputs()).some((e) => e.type === 'mouse-button'), 'no drag on the host');
  assert.equal(await client.isVisible('#zoom-reset'), true);
  // Zoomed in, a tap still lands on the host pixel under the finger.
  const vb = await client.locator('#video').boundingBox();
  const vw = await client.evaluate(() => [document.getElementById('video').videoWidth, document.getElementById('video').videoHeight]);
  const fit = Math.min(vb.width / vw[0], vb.height / vw[1]);
  const pic = { left: vb.x + (vb.width - vw[0] * fit) / 2, top: vb.y + (vb.height - vw[1] * fit) / 2, w: vw[0] * fit, h: vw[1] * fit };
  await clearLog();
  await client.touchscreen.tap(cx + 30, cy + 20);
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'mouse-button' && !e.down));
  const zabs = (await inputs()).find((e) => e.type === 'mouse-abs');
  const want = { x: (cx + 30 - pic.left) / pic.w * 65535, y: (cy + 20 - pic.top) / pic.h * 65535 };
  assert.ok(Math.abs(zabs.x - want.x) < 300 && Math.abs(zabs.y - want.y) < 300, JSON.stringify({ zabs, want }));
  await client.tap('#zoom-reset');

  // On-screen keyboard: text typed by an IME (no key codes) arrives as key presses, with Shift.
  await clearLog();
  await client.tap('#kbd-toggle');
  assert.ok(await client.evaluate(() => document.activeElement.id === 'kbd-input'));
  await client.keyboard.insertText('Hi');
  await host.waitForFunction(() => window.mockLog.filter((e) => e.type === 'key').length >= 6);
  got = (await inputs()).filter((e) => e.type === 'key').map((e) => [e.usage, e.down]);
  assert.deepEqual(got, [[0xe1, true], [0x0b, true], [0x0b, false], [0xe1, false], [0x0c, true], [0x0c, false]]);
  // Keys row: tapping it keeps the keyboard open; sticky Ctrl then Esc sends Ctrl+Esc.
  await clearLog();
  await client.tap('#keys-toggle');
  await client.tap('#keys [data-key="ControlLeft"]');
  await client.tap('#keys [data-key="Escape"]');
  await host.waitForFunction(() => window.mockLog.filter((e) => e.type === 'key').length >= 4);
  got = (await inputs()).filter((e) => e.type === 'key').map((e) => [e.usage, e.down]);
  assert.deepEqual(got, [[0xe0, true], [0x29, true], [0x29, false], [0xe0, false]]);
  assert.ok(await client.evaluate(() => document.activeElement.id === 'kbd-input'), 'keyboard stayed open');
  // A hardware shortcut while the keyboard is open goes through whole, not as Ctrl alone.
  await clearLog();
  await client.keyboard.press('Control+KeyC');
  await host.waitForFunction(() => window.mockLog.filter((e) => e.type === 'key').length >= 4);
  got = (await inputs()).filter((e) => e.type === 'key').map((e) => [e.usage, e.down]);
  assert.deepEqual(got, [[0xe0, true], [0x06, true], [0x06, false], [0xe0, false]]);
  assert.equal(await client.inputValue('#kbd-input').then((v) => v.replace(/\u200b/g, '')), '');
  // Backspace from the IME (deleting the hidden text) is a Backspace press.
  await clearLog();
  await client.keyboard.press('Backspace');
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'key' && e.usage === 0x2a && !e.down));
  await client.tap('#kbd-toggle');
  assert.ok(await client.evaluate(() => document.activeElement.id !== 'kbd-input'));
  // A sticky Ctrl ends with the next touch click, not just the next key.
  await clearLog();
  await client.tap('#keys [data-key="ControlLeft"]');
  await client.touchscreen.tap(cx, cy);
  await host.waitForFunction(() => window.mockLog.some((e) => e.type === 'key' && e.usage === 0xe0 && !e.down));
  got = (await inputs()).filter((e) => e.type === 'key' || e.type === 'mouse-button').map((e) => e.type === 'key' ? ['key', e.usage, e.down] : ['btn', e.button, e.down]);
  assert.deepEqual(got, [['key', 0xe0, true], ['btn', 0, true], ['btn', 0, false], ['key', 0xe0, false]]);
  assert.equal(await client.getAttribute('#keys [data-key="ControlLeft"]', 'aria-pressed'), 'false');

  // Fullscreen from the overlay; Exit brings the toolbar back.
  await client.tap('#touch-fullscreen');
  await client.waitForFunction(() => document.fullscreenElement?.id === 'stage' || document.body.classList.contains('fake-fs'));
  await client.tap('#touch-fullscreen');
  await client.waitForFunction(() => !document.fullscreenElement && !document.body.classList.contains('fake-fs'));

  assert.deepEqual(errors, []);
});
