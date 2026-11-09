// Runs the test host (cargo build -p ws-host --example test-host --features synthetic) and pairs
// a browser with it, for the media tests. The host's output is kept so a test can wait on it.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright';

export const bin = process.env.WS_HOST_BIN;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export async function startHost(t, args = []) {
  const dir = await mkdtemp(join(tmpdir(), 'ws-media-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const port = 20000 + Math.floor(Math.random() * 20000);
  const proc = spawn(bin, ['--port', String(port), '--no-ice-tcp', '--data-dir', join(dir, 'data'), '--ip', '127.0.0.1',
    '--web-root', fileURLToPath(new URL('../..', import.meta.url)), ...args], { stdio: ['pipe', 'pipe', 'pipe'] });
  t.after(() => proc.kill());
  const host = { port, proc, out: '' };
  proc.stdout.on('data', (d) => { host.out += d; });
  proc.stderr.on('data', (d) => { host.out += d; });
  // Waits for `re` in what the host printed after `from`, and returns the match.
  host.says = async (re, from = 0, timeoutMs = 30000) => {
    for (const end = Date.now() + timeoutMs; Date.now() < end;) {
      const m = host.out.slice(from).match(re);
      if (m) return m;
      if (proc.exitCode != null) break;
      await sleep(50);
    }
    assert.fail(`host never said ${re}:\n${host.out.slice(-3000)}`);
  };
  host.type = (line) => proc.stdin.write(line + '\n');
  await host.says(/signaling listening/);
  return host;
}

// Google Chrome by default: Playwright's own Chromium has no H.264, which the synthetic source
// sends. CHROME_PATH picks another binary.
export async function launchBrowser(t) {
  const browser = await chromium.launch({
    ...(process.env.CHROME_PATH ? { executablePath: process.env.CHROME_PATH } : { channel: 'chrome' }),
    args: ['--no-proxy-server', '--autoplay-policy=no-user-gesture-required', '--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream',
      '--disable-background-timer-throttling', '--disable-renderer-backgrounding', '--disable-backgrounding-occluded-windows'],
  });
  t.after(() => browser.close());
  return browser;
}

// Opens the client in a new context with the probe loaded, pairs it and waits for the stream.
// `proxyPort` sends the media through an impairment proxy (impair.js) in front of the host.
export async function connect(browser, host, { proxyPort, settings } = {}) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 }, ignoreHTTPSErrors: true });
  const page = await ctx.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  if (settings) {
    await page.addInitScript((s) => {
      try { localStorage.setItem('ws-stream-settings', JSON.stringify(s)); } catch {}
    }, settings);
  }
  await page.addInitScript((c) => { window.__probeConfig = c; }, { mediaPort: host.port, proxyPort });
  await page.addInitScript({ content: await readFile(new URL('./stamp.js', import.meta.url), 'utf8') });
  await page.addInitScript({ content: await readFile(new URL('./probe.js', import.meta.url), 'utf8') });
  await page.goto(`https://127.0.0.1:${host.port}/?autoconnect`);
  const from = host.out.length;
  host.type('pair');
  await page.click('#pair-start');
  const [, code] = await host.says(/PAIRING CODE (\d+)/, from);
  await page.fill('#pair-code', code);
  await page.click('#pair-confirm');
  await page.waitForFunction(() => window.__probe.frames.filter((f) => !f.bad).length > 30, null, { timeout: 30000 });
  return { ctx, page, errors };
}

// Everything the probe has collected so far.
export const collect = (page) => page.evaluate(() => ({ frames: window.__probe.frames, beeps: window.__probe.beeps }));
export const now = (page) => page.evaluate(() => performance.timeOrigin + performance.now());
