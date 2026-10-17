// Drives headless Chromium against the test_pattern example and checks that
// pairing, signaling, video decode and every data channel work, over UDP and
// then ICE-TCP. Usage: node run.mjs <port> (needs the `playwright` package).
import { spawn } from 'node:child_process';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_PATH || 'playwright');
const port = process.argv[2] || '18443';
const origin = `https://127.0.0.1:${port}`;

const codes = [];
const host = spawn(process.env.TEST_PATTERN_BIN, [port], { stdio: ['ignore', 'pipe', 'inherit'] });
host.stdout.on('data', (d) => {
  for (const line of d.toString().split('\n')) {
    const m = line.match(/^PAIRING_CODE (\d{6})/);
    if (m) codes.push(m[1]);
  }
});
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const fail = (msg) => { console.error('FAIL:', msg); host.kill(); process.exit(1); };

await wait(1500);
const browser = await chromium.launch();
const ctx = await browser.newContext({ ignoreHTTPSErrors: true });
const page = await ctx.newPage();
await page.goto(origin + '/index.html');

const id = await page.evaluate(() => window.startPair());
for (let i = 0; i < 50 && !codes.length; i++) await wait(100);
if (!codes.length) fail('no pairing code printed');
const status = await page.evaluate(([id, code]) => window.confirmPair(id, code), [id, codes[0]]);
if (status !== 200) fail('pairing confirm returned ' + status);

async function check(tcpOnly) {
  await page.evaluate((t) => window.connect(t), tcpOnly);
  let s;
  for (let i = 0; i < 100; i++) {
    await wait(100);
    s = await page.evaluate(() => window.stats());
    if (s.framesDecoded > 10 && Object.keys(s.channels).length === 4 && Object.values(s.channels).every((c) => c.state === 'open')) break;
  }
  await page.evaluate(() => { for (const ch of Object.values(window.e2e.channels)) ch.send('hi-' + ch.label); });
  await wait(500);
  s = await page.evaluate(() => window.stats());
  console.log(JSON.stringify({ tcpOnly, ...s }, null, 1));
  if (!(s.framesDecoded > 10)) fail('video not decoding');
  if (s.localProtocol !== (tcpOnly ? 'tcp' : 'udp')) fail('wrong transport ' + s.localProtocol);
  if (s.channels['input-motion']?.ordered !== false || s.channels['input-motion']?.maxRetransmits !== 0) fail('input-motion options');
  for (const label of ['control', 'input', 'input-motion', 'file']) {
    if (!s.echo.some((e) => e.label === label && e.data === 'echo:hi-' + label)) fail('no echo on ' + label);
  }
  if (!s.offerExtPlayoutDelay) fail('offer lacks playout-delay');
  await page.evaluate(() => { window.e2e.pc.close(); window.e2e.ws.close(); window.e2e.echo = []; window.e2e.channels = {}; });
  await wait(500);
}

await check(false);
await check(true);
console.log('PASS');
await browser.close();
host.kill();
