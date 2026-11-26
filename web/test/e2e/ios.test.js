// End-to-end in Safari's engine: an iPhone client (WebKit) saves two PCs, streams, swaps and opens offline.
// Runs with E2E_WEBKIT=1 (CI installs WebKit); the mock hosts run in Chromium.
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { chromium, webkit, devices } from 'playwright';

const serverJs = fileURLToPath(new URL('../../dev/server.mjs', import.meta.url));
const skip = process.env.E2E_WEBKIT !== '1' && 'set E2E_WEBKIT=1 to run in WebKit';

async function devServer(t, port) {
  const server = spawn(process.execPath, [serverJs, String(port)], { stdio: ['ignore', 'pipe', 'inherit'] });
  t.after(() => server.kill());
  await new Promise((r) => server.stdout.once('data', r));
  return server;
}

async function mockHost(ctx, base) {
  const page = await ctx.newPage();
  await page.goto(`${base}/dev/mock-host.html`);
  await page.waitForFunction(() => window.mockLog?.some((e) => e.ev === 'signaling-open'));
  return page;
}

async function streaming(page) {
  try {
    await page.waitForFunction(() => {
      const v = document.getElementById('video');
      return document.getElementById('status').dataset.state === 'connected' && v.videoWidth > 0 && !v.paused;
    }, null, { timeout: 30000 });
  } catch (e) {
    const state = await page.evaluate(() => {
      const v = document.getElementById('video');
      return {
        status: document.getElementById('status').textContent,
        state: document.getElementById('status').dataset.state,
        videoWidth: v.videoWidth, paused: v.paused, muted: v.muted, readyState: v.readyState,
        tracks: v.srcObject?.getTracks().map((t) => `${t.kind}:${t.readyState}`) ?? null,
        rtc: typeof RTCPeerConnection, pair: document.getElementById('pair-msg')?.textContent,
      };
    }).catch((err) => ({ evaluate: err.message }));
    throw new Error(`${e.message}\nclient: ${JSON.stringify(state)}\nconsole: ${logs.slice(-20).join(' | ')}`);
  }
}
const logs = [];

test('iPhone (WebKit): saved PCs, streaming, swapping and offline start', { skip, timeout: 180000 }, async (t) => {
  const portA = 20000 + Math.floor(Math.random() * 500);
  const portB = portA + 500;
  const baseA = `http://localhost:${portA}`;
  const baseB = `http://127.0.0.1:${portB}`;
  const serverA = await devServer(t, portA);
  await devServer(t, portB);

  const hosts = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required', '--disable-background-timer-throttling', '--disable-renderer-backgrounding', '--disable-backgrounding-occluded-windows'] });
  t.after(() => hosts.close());
  const hostCtx = await hosts.newContext();
  const hostA = await mockHost(hostCtx, baseA);
  const hostB = await mockHost(hostCtx, baseB);

  const browser = await webkit.launch();
  t.after(() => browser.close());
  const ctx = await browser.newContext({ ...devices['iPhone 13 landscape'] });
  const client = await ctx.newPage();
  client.on('console', (m) => logs.push(`${m.type()}: ${m.text()}`));
  hostA.on('console', (m) => logs.push(`hostA ${m.type()}: ${m.text()}`));
  const errors = [];
  client.on('pageerror', (e) => errors.push(e.message));
  await client.goto(`${baseA}/`);

  // This PC first, by code.
  await client.waitForSelector('#pair:not([hidden])');
  await client.tap('#pair-start');
  await hostA.waitForFunction(() => window.mockPairingCode);
  await client.fill('#pair-code', await hostA.evaluate(() => window.mockPairingCode));
  await client.tap('#pair-confirm');
  await streaming(client);

  // A second PC, saved with its token.
  await client.tap('#connect');
  await client.tap('#pc-add');
  await client.fill('#add-address', baseB);
  await client.tap('#add-start');
  await hostB.waitForFunction(() => window.mockPairingCode);
  await client.fill('#add-code', await hostB.evaluate(() => window.mockPairingCode));
  await client.tap('#add-confirm');
  await streaming(client);
  const saved = await client.evaluate(() => JSON.parse(localStorage.getItem('ws-hosts')));
  assert.equal(saved.length, 1);
  assert.ok(saved[0].token);

  // Swap back and forth with the toolbar switcher.
  await client.selectOption('#pc-quick', 'local');
  await streaming(client);
  await client.selectOption('#pc-quick', saved[0].id);
  await streaming(client);

  // The touch overlay and the on-screen keyboard row work in WebKit.
  assert.equal(await client.isVisible('#touchbar'), true);
  await client.tap('#keys-toggle');
  assert.equal(await client.isVisible('#keys'), true);

  // Offline start: the page's own PC goes away, the app opens from cache and reconnects to the other.
  await client.evaluate(() => navigator.serviceWorker?.ready);
  serverA.kill();
  await new Promise((r) => serverA.once('exit', r));
  await client.reload();
  await streaming(client);
  assert.deepEqual(errors, []);
});
