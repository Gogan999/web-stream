// End-to-end against the real host: the pairing link the host's QR code holds pairs a phone and
// connects it, once. Runs when WS_HOST_BIN names a built test host (see download.test.js).
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium, devices } from 'playwright';

const bin = process.env.WS_HOST_BIN;

test('the pairing link pairs and connects once', { skip: !bin && 'WS_HOST_BIN not set', timeout: 120000 }, async (t) => {
  const dir = await mkdtemp(join(tmpdir(), 'ws-pair-link-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const port = 22000 + Math.floor(Math.random() * 2000);
  const host = spawn(bin, ['serve', '--port', String(port), '--data-dir', join(dir, 'data'), '--ip', '127.0.0.1', '--no-audio', '--no-mic', '--web-root', fileURLToPath(new URL('../..', import.meta.url))], { stdio: ['pipe', 'pipe', 'pipe'] });
  t.after(() => host.kill());
  let out = '';
  host.stdout.on('data', (d) => { out += d; });
  host.stderr.on('data', (d) => { out += d; });
  const hostSays = async (re, from = 0) => {
    for (let i = 0; i < 300; i++) {
      const m = out.slice(from).match(re);
      if (m) return m;
      await new Promise((r) => setTimeout(r, 50));
    }
    assert.fail(`host never said ${re}:\n${out.slice(-2000)}`);
  };
  await hostSays(/signaling listening/);

  const browser = await chromium.launch();
  t.after(() => browser.close());

  let from = out.length;
  host.stdin.write('pair\n');
  const [, link] = await hostSays(/PAIRING LINK #pair=([A-Za-z0-9_-]+)/, from);

  const phone = await browser.newContext({ ...devices['iPhone 13'], ignoreHTTPSErrors: true });
  const page = await phone.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  from = out.length;
  await page.goto(`https://127.0.0.1:${port}/#pair=${link}`);
  await hostSays(/device paired/, from);
  await hostSays(/handed off to the peer connection/, from);
  // The secret is gone from the address bar.
  assert.equal(new URL(page.url()).hash, '');
  assert.deepEqual(errors, []);

  // A second browser can't reuse it. (One at a time: the host allows 8 connections per address.)
  await phone.close();
  const other = await browser.newContext({ ignoreHTTPSErrors: true });
  const page2 = await other.newPage();
  await page2.goto(`https://127.0.0.1:${port}/#pair=${link}`);
  await page2.waitForFunction(() => /expired or was used/.test(document.getElementById('pair-msg').textContent));
  assert.equal(new URL(page2.url()).hash, '');
  await other.close();
});
