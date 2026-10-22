// End-to-end against the real host: a file shared from the test host's console reaches the browser's
// downloads, or its save dialog. Runs when WS_HOST_BIN names a built test host (CI's Linux job;
// cargo build -p ws-host --example test-host);
// display capture isn't there, but pairing, the control and file channels are.
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { randomBytes } from 'node:crypto';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium, devices } from 'playwright';

const bin = process.env.WS_HOST_BIN;

test('a shared file opens a download or the save dialog', { skip: !bin && 'WS_HOST_BIN not set', timeout: 120000 }, async (t) => {
  const dir = await mkdtemp(join(tmpdir(), 'ws-download-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const port = 20000 + Math.floor(Math.random() * 2000);
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
  const type = (line) => host.stdin.write(line + '\n');
  await hostSays(/signaling listening/);

  const browser = await chromium.launch();
  t.after(() => browser.close());

  // Pairs a new browser context and connects it. dialog: 'none' (no save dialog), 'ok' or
  // 'blocked' (it refuses to open).
  async function connect(device, dialog = 'none') {
    const ctx = await browser.newContext({ ...device, ignoreHTTPSErrors: true, acceptDownloads: true });
    const page = await ctx.newPage();
    const errors = [];
    page.on('pageerror', (e) => errors.push(e.message));
    // Chrome's save dialog can't open headless: a stand-in records whether the Save click's
    // activation was still live, and keeps what is written.
    await page.addInitScript((dialog) => {
      window.picks = [];
      window.showSaveFilePicker = dialog === 'none' ? undefined : async ({ suggestedName }) => {
        window.picks.push({ suggestedName, active: navigator.userActivation.isActive });
        if (dialog === 'blocked') throw new DOMException('blocked by policy', 'NotAllowedError');
        const parts = [];
        return {
          createWritable: async () => ({
            write: async (d) => { parts.push(d.slice()); },
            close: async () => { window.written = new Uint8Array(await new Blob(parts).arrayBuffer()); },
            abort: async () => {},
          }),
        };
      };
    }, dialog);
    await page.goto(`https://127.0.0.1:${port}/?autoconnect`);
    const from = out.length;
    type('pair');
    await page.click('#pair-start');
    const [, code] = await hostSays(/PAIRING CODE (\d+)/, from);
    await page.fill('#pair-code', code);
    await page.click('#pair-confirm');
    // Files can be shared once the control channel is up, which is when signaling hands off.
    await hostSays(/handed off to the peer connection/, from);
    return { ctx, page, errors };
  }

  // Shares a new file and waits for its row.
  let n = 0;
  async function share(page, size) {
    const data = randomBytes(size);
    const name = `report ${++n}.pdf`;
    await writeFile(join(dir, name), data);
    const from = out.length;
    type(`send ${join(dir, name)}`);
    await hostSays(/sharing/, from);
    await page.waitForSelector(`#file-list li:has-text("${name}")`, { state: 'attached' });
    return { data, name, row: `#file-list li:has-text("${name}")` };
  }
  async function saveToDownloads(page, { data, name, row }) {
    if (!(await page.isVisible('#files'))) await page.click('#files-toggle');
    const [download] = await Promise.all([page.waitForEvent('download'), page.click(`${row} button`)]);
    assert.equal(download.suggestedFilename(), name);
    assert.ok(data.equals(await readFile(await download.path())), `${name} arrived intact`);
    await page.waitForSelector(`${row} .info:has-text("Saved")`);
  }

  const desktop = { viewport: { width: 1280, height: 800 } };

  // No save dialog (Firefox, Safari): collected in memory, then a download.
  let s = await connect(desktop);
  await saveToDownloads(s.page, await share(s.page, 3 * 1024 * 1024 + 5));
  assert.deepEqual(s.errors, []);
  await s.ctx.close();

  // Chrome's save dialog: opened while the click still counts, and gets every byte.
  s = await connect(desktop, 'ok');
  const viaDialog = await share(s.page, 2 * 1024 * 1024);
  await s.page.click('#files-toggle');
  await s.page.click(`${viaDialog.row} button`);
  await s.page.waitForSelector(`${viaDialog.row} .info:has-text("Saved")`);
  assert.deepEqual(await s.page.evaluate(() => window.picks), [{ suggestedName: viaDialog.name, active: true }]);
  assert.ok(viaDialog.data.equals(Buffer.from(await s.page.evaluate(() => [...window.written]))));
  await s.ctx.close();

  // A save dialog that won't open (policy, say) still gets the file to downloads.
  s = await connect(desktop, 'blocked');
  await saveToDownloads(s.page, await share(s.page, 100 * 1024));
  assert.equal((await s.page.evaluate(() => window.picks)).length, 1);
  await s.ctx.close();

  // A phone (Android has no save dialog): from the toolbar's Files button to downloads.
  s = await connect(devices['Pixel 7']);
  await saveToDownloads(s.page, await share(s.page, 1024 * 1024));
  assert.deepEqual(s.errors, []);
});
