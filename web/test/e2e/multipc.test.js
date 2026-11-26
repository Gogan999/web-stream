// End-to-end: one page saves a second PC, connects to it, and swaps between PCs.
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright';

const serverJs = fileURLToPath(new URL('../../dev/server.mjs', import.meta.url));

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

const connected = (page) => page.waitForFunction(() => document.getElementById('status').dataset.state === 'connected'
  && document.getElementById('video').videoWidth > 0, null, { timeout: 20000 });

test('a saved second PC pairs by token, connects, survives a reload and swaps', { timeout: 90000 }, async (t) => {
  const portA = 19000 + Math.floor(Math.random() * 500);
  const portB = portA + 500;
  const baseA = `http://127.0.0.1:${portA}`;
  const baseB = `http://localhost:${portB}`;
  const serverA = await devServer(t, portA);
  await devServer(t, portB);

  const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required', '--disable-background-timer-throttling', '--disable-renderer-backgrounding', '--disable-backgrounding-occluded-windows'] });
  t.after(() => browser.close());
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const hostA = await mockHost(ctx, baseA);
  const hostB = await mockHost(ctx, baseB);

  const client = await ctx.newPage();
  const errors = [];
  client.on('pageerror', (e) => errors.push(e.message));
  await client.goto(`${baseA}/`);

  await client.waitForSelector('#pair:not([hidden])');
  await client.click('#pair-other');
  await client.waitForSelector('#add-pc:not([hidden])');
  await client.fill('#add-address', baseB);
  await client.click('#add-start');
  await hostB.waitForFunction(() => window.mockPairingCode);
  const codeB = await hostB.evaluate(() => window.mockPairingCode);
  await client.fill('#add-code', codeB);
  await client.click('#add-confirm');
  await connected(client);
  assert.ok(await hostB.evaluate(() => window.mockLog.some((e) => e.ev === 'pc-connected' || e.ev === 'connected' || /connect/.test(e.ev))));

  const saved = await client.evaluate(() => JSON.parse(localStorage.getItem('ws-hosts')));
  assert.equal(saved.length, 1);
  assert.equal(saved[0].origin, baseB);
  assert.ok(saved[0].token);

  // Reload: the saved PC is offered and autoconnect picks it again.
  await client.reload();
  await connected(client);
  assert.equal(await client.locator('#pair').isHidden(), true);

  // Pair the page's own PC too, then swap with the toolbar switcher.
  await client.click('#connect');
  await client.waitForSelector('#pc-list li');
  await client.click('#pc-list button.pc-connect:has-text("Pair")');
  await client.waitForSelector('#pair:not([hidden])');
  await client.click('#pair-start');
  await hostA.waitForFunction(() => window.mockPairingCode);
  await client.fill('#pair-code', await hostA.evaluate(() => window.mockPairingCode));
  await client.click('#pair-confirm');
  await connected(client);
  assert.equal(await client.locator('#pc-quick').isVisible(), true);
  const options = await client.$$eval('#pc-quick option', (os) => os.map((o) => o.value));
  assert.equal(options.length, 2);
  assert.equal(await client.inputValue('#pc-quick'), 'local');
  await client.selectOption('#pc-quick', saved[0].id);
  await connected(client);
  assert.equal(await client.evaluate(() => JSON.parse(localStorage.getItem('ws-host-last'))), saved[0].id);

  // Forgetting the saved PC removes it from the list and the switcher.
  await client.click('#connect');
  client.once('dialog', (d) => d.accept());
  await client.click('#pc-list .pc-remove');
  await client.waitForFunction(() => document.querySelectorAll('#pc-quick option').length === 1);
  assert.deepEqual(await client.evaluate(() => JSON.parse(localStorage.getItem('ws-hosts'))), []);
  assert.deepEqual(errors, []);
});

test('the app opens from cache when its own PC is off, and offers the saved PCs', { timeout: 90000 }, async (t) => {
  const portA = 19000 + Math.floor(Math.random() * 500);
  const portB = portA + 500;
  const baseA = `http://localhost:${portA}`;
  const baseB = `http://127.0.0.1:${portB}`;
  const serverA = await devServer(t, portA);
  await devServer(t, portB);
  const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required'] });
  t.after(() => browser.close());
  const ctx = await browser.newContext({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
  const hostB = await mockHost(ctx, baseB);
  const client = await ctx.newPage();
  await client.goto(`${baseA}/`);
  await client.evaluate(() => navigator.serviceWorker.ready);
  await client.click('#pair-other');
  await client.fill('#add-address', baseB);
  await client.click('#add-start');
  await hostB.waitForFunction(() => window.mockPairingCode);
  await client.fill('#add-code', await hostB.evaluate(() => window.mockPairingCode));
  await client.click('#add-confirm');
  await connected(client);
  await client.evaluate(() => caches.keys()).then((k) => assert.ok(k.length > 0));

  serverA.kill();
  await new Promise((r) => serverA.once('exit', r));
  await client.reload();
  await connected(client);
  assert.match(await client.textContent('#status'), /Connected/);
});
