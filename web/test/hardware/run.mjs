// One command that checks the real host on a real Windows PC (or any machine with the host built
// and displays attached): video, display switching, game audio, input injection, the mic, and
// the same again with the GPU under load. Writes a JSON report and prints a summary.
//
//   cargo build --release -p ws-host --example test-host
//   cd web && npm ci
//   node test/hardware/run.mjs --displays "0,0;2560,0" --gpu-load
//
// How it works: for each display a Chrome window is opened full screen on it (source.html): it
// draws a stamp (capture time, display, frame index) over moving noise and beeps at the top of
// every Unix second. The host streams those displays, a second Chrome (headless) is the client,
// reads the stamps in what it receives (lib/probe.js), and sends mouse and keys, which the page
// on the streamed display records when the host injects them. The mic path plays the client's
// fake microphone into the host's virtual cable, and a third page listens on the cable's other
// end. It moves the real mouse pointer and steals the displays for the length of the run
// (a minute or two), so leave the machine alone. Windows Firewall may ask about test-host.exe the
// first time: Cancel is fine, everything runs on this machine.
//
// Options:
//   --host-bin FILE     the host (default target/release/examples/test-host[.exe])
//   --host-arg ARG      an extra host option, repeatable (e.g. --host-arg --display --host-arg 1)
//   --displays "x,y;..." top-left corner of each display in desktop coordinates, in the order the
//                       host lists them (default "0,0": one display). The host's own log line
//                       "streaming display=..." and Windows' display settings show them.
//   --size WxH --fps N --kbps N   what the client asks for (default 1920x1080 60 20000)
//   --seconds N         length of each steady measurement (default 20)
//   --gpu-load          repeat the measurements with a heavy shader running on --load-at
//   --load-at "x,y"     where the load window goes (default: the last display)
//   --skip LIST         comma list of: switch,input,mic,audio
//   --chrome FILE       a Chrome to use instead of the installed Google Chrome
//   --report FILE       where the JSON goes (default hardware-report.json)
//   --synthetic[=N]     development: stream the host's synthetic source (needs a host built with
//                       --features synthetic) and skip the pages, input and mic
import { mkdir, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { chromium } from 'playwright';
import { collect, connect, now, startHost } from '../lib/host.js';
import { summarize } from '../lib/metrics.js';

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, '../../..');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function parseArgs(argv) {
  const o = { hostArgs: [], displays: '0,0', size: '1920x1080', fps: 60, kbps: 20000, seconds: 20, gpuLoad: false, skip: [], report: 'hardware-report.json' };
  for (let i = 0; i < argv.length; i++) {
    const [flag, inline] = argv[i].split(/=(.*)/s);
    const val = () => inline ?? argv[++i];
    switch (flag) {
      case '--host-bin': o.hostBin = val(); break;
      case '--host-arg': o.hostArgs.push(val()); break;
      case '--displays': o.displays = val(); break;
      case '--size': o.size = val(); break;
      case '--fps': o.fps = Number(val()); break;
      case '--kbps': o.kbps = Number(val()); break;
      case '--seconds': o.seconds = Number(val()); break;
      case '--gpu-load': o.gpuLoad = true; break;
      case '--load-at': o.loadAt = val(); break;
      case '--skip': o.skip = val().split(','); break;
      case '--chrome': o.chrome = val(); break;
      case '--report': o.report = val(); break;
      case '--synthetic': o.synthetic = Number(inline || 2); break;
      default: throw new Error(`unknown option ${argv[i]}`);
    }
  }
  const [w, h] = o.size.split('x').map(Number);
  if (!w || !h) throw new Error('--size is WxH');
  o.height = h;
  o.positions = o.displays.split(';').map((p) => p.split(',').map(Number));
  if (o.positions.some((p) => p.length !== 2 || p.some(Number.isNaN))) throw new Error('--displays is "x,y;x,y"');
  return o;
}

const opts = parseArgs(process.argv.slice(2));
const cleanups = [];
const t = { after: (fn) => cleanups.push(fn) }; // the shape lib/host.js wants
const report = { startedAt: new Date().toISOString(), options: { ...opts }, problems: [], phases: {} };
const problem = (msg) => { report.problems.push(msg); console.log(`PROBLEM: ${msg}`); };
const say = (msg) => console.log(msg);

async function launch(extraArgs = [], { headless = false } = {}) {
  const browser = await chromium.launch({
    ...(opts.chrome ? { executablePath: opts.chrome } : { channel: 'chrome' }),
    headless,
    args: ['--no-proxy-server', '--autoplay-policy=no-user-gesture-required', '--use-fake-ui-for-media-stream',
      '--disable-background-timer-throttling', '--disable-renderer-backgrounding', '--disable-backgrounding-occluded-windows',
      '--disable-infobars', '--no-first-run', ...extraArgs],
  });
  cleanups.push(() => browser.close().catch(() => {}));
  return browser;
}

// A full-screen Chrome on the display whose top-left corner is (x, y), showing `page` of this folder.
async function openOn(x, y, file, query = '') {
  const browser = await launch([`--window-position=${x},${y}`, '--window-size=800,600', '--kiosk']);
  const ctx = await browser.newContext({ viewport: null });
  const page = await ctx.newPage();
  page.on('pageerror', (e) => problem(`${file}: ${e.message}`));
  await page.goto(`${pathToFileURL(resolve(here, file)).href}${query}`);
  return page;
}

// Steady measurement of the client's stream: the stamps (latency, freezes, which display) and the
// browser's own counters (what it decoded, dropped, lost), over `ms`.
async function measure(client, ms) {
  const before = await client.evaluate(() => window.__probe.stats());
  const from = await now(client);
  await sleep(ms);
  const to = await now(client);
  const after = await client.evaluate(() => window.__probe.stats());
  const s = summarize(await collect(client), from, to);
  const v0 = before.video, v1 = after.video, secs = (to - from) / 1000;
  return {
    ...s,
    receivedFps: v1 && v0 ? +((v1.framesDecoded - v0.framesDecoded) / secs).toFixed(1) : null,
    receivedMbps: v1 && v0 ? +(((v1.bytesReceived - v0.bytesReceived) * 8) / secs / 1e6).toFixed(2) : null,
    framesDropped: v1.framesDropped - v0.framesDropped,
    freezeCount: v1.freezeCount - v0.freezeCount,
    packetsLost: v1.packetsLost - v0.packetsLost,
    keyFrames: v1.keyFramesDecoded - v0.keyFramesDecoded,
    rttMs: after.rttMs,
    decoder: v1.decoderImplementation,
    jitterBufferMs: v1.jitterBufferEmittedCount ? +((v1.jitterBufferDelay / v1.jitterBufferEmittedCount) * 1000).toFixed(1) : null,
  };
}

// Switches the client through `order` (display ids) and times each switch from the click to the
// first frame stamped with the new display.
async function switching(client, order) {
  const out = [];
  if (!(await client.isVisible('#display'))) await client.click('#more-toggle');
  for (const id of order) {
    const at = await now(client);
    await client.selectOption('#display', String(id));
    try {
      await client.waitForFunction(([id, at]) => window.__probe.frames.some((f) => f.display === id && f.shown > at), [id, at], { timeout: 10000 });
    } catch {
      problem(`display ${id} never showed after selecting it`);
      out.push({ to: id, switchMs: null });
      continue;
    }
    await sleep(1500);
    const data = await collect(client);
    const first = data.frames.find((f) => f.display === id && f.shown > at);
    const after = summarize(data, first.shown, first.shown + 1500);
    out.push({ to: id, switchMs: Math.round(first.shown - at), mixed: after.displays.length > 1, fps: after.fps, freezes: after.freezes, longestFreezeMs: after.longestFreezeMs });
  }
  return out;
}

async function run() {
  const synthetic = opts.synthetic != null;
  const exe = opts.hostBin ?? resolve(repo, 'target/release/examples', process.platform === 'win32' ? 'test-host.exe' : 'test-host');
  if (!existsSync(exe)) throw new Error(`host not found at ${exe}: build it (cargo build --release -p ws-host --example test-host) or pass --host-bin`);
  const [w, h] = opts.size.split('x').map(Number);
  const hostArgs = ['--size', opts.size, '--fps', String(opts.fps), '--kbps', String(opts.kbps), ...(synthetic ? [`--synthetic=${opts.synthetic}`] : []), ...opts.hostArgs];

  // 1. Source pages on every display, first, so there is something to capture.
  const sources = [];
  if (!synthetic) {
    for (const [i, [x, y]] of opts.positions.entries()) sources.push(await openOn(x, y, 'source.html', `?display=${i}`));
    await sources[0].evaluate(() => window.__startAudio());
    await sleep(1000);
  }

  // 2. The host (every LAN address, as it runs for real) and the client.
  const host = await startHost(t, hostArgs, { bin: exe, ip: null, iceTcp: true });
  report.hostArgs = hostArgs;
  const clientBrowser = await launch(['--use-fake-device-for-media-stream'], { headless: true });
  const settings = { resolution: String(opts.height), fps: opts.fps, bitrate: String(opts.kbps), mouseMode: 'absolute', mic: false };
  const { page: client, errors } = await connect(clientBrowser, host, { settings });
  const [, config] = host.out.match(/streaming display=\d+ config=(.*)/) ?? [];
  report.streamingConfig = config ?? null;
  say(`connected; host says: ${config ?? '(no streaming line yet)'}`);
  await sleep(3000);

  // 3. Steady video and audio.
  say(`measuring ${opts.seconds} s of steady stream...`);
  const steady = await measure(client, opts.seconds * 1000);
  report.phases.steady = steady;
  say(`  ${steady.receivedFps} fps received, ${steady.receivedMbps} Mb/s, latency p50 ${steady.latencyMs.p50} / p95 ${steady.latencyMs.p95} ms, ${steady.freezes} freezes, size ${steady.size}`);
  if (!steady.frames) problem('no readable frames: is the source page on the streamed display, fullscreen?');
  if (!opts.skip.includes('audio') && !synthetic) {
    if (steady.beeps < 3) problem(`heard ${steady.beeps} beeps in ${opts.seconds} s of game audio (is there a default output device?)`);
    else say(`  audio: ${steady.beeps} beeps, ${steady.audioLatencyMs.p50} ms late, sound ${steady.avOffsetMs} ms behind the picture`);
  }

  // 4. Display switching.
  const ids = Array.from({ length: synthetic ? opts.synthetic : opts.positions.length }, (_, i) => i);
  if (!opts.skip.includes('switch') && ids.length > 1) {
    report.phases.switch = await switching(client, [1, 0, 1, 0]);
    say(`display switches: ${report.phases.switch.map((s) => `${s.to}:${s.switchMs}ms${s.mixed ? ' MIXED' : ''}`).join(' ')}`);
  } else if (!opts.skip.includes('switch')) {
    say('display switching skipped: only one display in --displays');
  }

  // 5. Input: mouse and keys from the client, as the page on display 0 sees them.
  if (!opts.skip.includes('input') && !synthetic) {
    if (ids.length > 1) await client.selectOption('#display', '0');
    await sleep(1500);
    await sources[0].evaluate(() => { window.__events.length = 0; });
    const box = await client.locator('#video').boundingBox();
    await client.mouse.move(box.x + box.width * 0.2, box.y + box.height * 0.5);
    await client.mouse.move(box.x + box.width * 0.8, box.y + box.height * 0.6, { steps: 40 });
    await client.mouse.down(); await client.mouse.up();
    await client.keyboard.press('q'); await client.keyboard.press('w');
    await client.mouse.wheel(0, 120);
    await sleep(1500);
    const events = await sources[0].evaluate(() => ({ events: window.__events, last: window.__lastMove }));
    const count = (type) => events.events.filter((e) => e.type === type).length;
    const keys = new Set(events.events.filter((e) => e.type === 'keydown').map((e) => e.code));
    report.phases.input = { mousemoves: events.last?.moves ?? 0, mousedown: count('mousedown'), mouseup: count('mouseup'), wheel: count('wheel'), keys: [...keys], lastMove: events.last };
    say(`input: ${report.phases.input.mousemoves} mouse moves, ${report.phases.input.mousedown} clicks, keys ${[...keys].join(',') || 'none'}, ${report.phases.input.wheel} wheel`);
    if (!report.phases.input.mousemoves) problem('no mouse movement reached the page on display 0 (is it under another window?)');
    if (!report.phases.input.mousedown) problem('no mouse click reached the page');
    if (!keys.has('KeyQ') || !keys.has('KeyW')) problem(`keys did not reach the page (got ${[...keys].join(',') || 'none'})`);
  }

  // 6. Mic: the client's fake microphone into the host's virtual cable, listened to at its other end.
  if (!opts.skip.includes('mic') && !synthetic) {
    const listenBrowser = await launch([], { headless: true });
    const lctx = await listenBrowser.newContext();
    const listen = await lctx.newPage();
    await listen.goto(`${pathToFileURL(resolve(here, 'mic-listen.html')).href}`);
    await listen.waitForFunction(() => window.__mic.state !== 'starting', null, { timeout: 15000 });
    const state = await listen.evaluate(() => window.__mic.state);
    if (state !== 'listening') {
      report.phases.mic = { state };
      say(`mic skipped: listener ${state} (install VB-CABLE, or the host has no virtual mic device)`);
    } else {
      await client.click('#more-toggle').catch(() => {});
      if (!(await client.isChecked('#mic'))) await client.evaluate(() => document.getElementById('mic').click());
      await sleep(8000);
      const mic = await listen.evaluate(() => window.__mic);
      const peak = Math.max(0, ...mic.levels);
      report.phases.mic = { state, device: mic.device, peakPerSecond: mic.levels, peak, hostSaid: (host.out.match(/mic playback started[^\n]*/) ?? [null])[0] };
      say(`mic: peak ${peak} on "${mic.device}"`);
      if (peak < 0.02) problem('nothing came out of the virtual cable while the browser mic was on');
      await client.evaluate(() => { if (document.getElementById('mic').checked) document.getElementById('mic').click(); });
    }
  }

  // 7. The same under GPU load.
  if (opts.gpuLoad) {
    const [lx, ly] = (opts.loadAt ?? opts.positions.at(-1).join(',')).split(',').map(Number);
    const load = await openOn(lx, ly, 'load.html');
    await sleep(4000);
    say(`GPU load running (${await load.evaluate(() => window.__loadFrames)} frames so far); measuring...`);
    report.phases.loaded = await measure(client, opts.seconds * 1000);
    const l = report.phases.loaded;
    say(`  under load: ${l.receivedFps} fps, latency p50 ${l.latencyMs.p50} / p95 ${l.latencyMs.p95} ms, ${l.freezes} freezes, ${l.framesDropped} dropped`);
    if (!opts.skip.includes('switch') && ids.length > 1) {
      report.phases.loadedSwitch = await switching(client, [1, 0, 1, 0]);
      say(`display switches under load: ${report.phases.loadedSwitch.map((s) => `${s.to}:${s.switchMs}ms`).join(' ')}`);
    }
    await load.context().browser().close().catch(() => {});
  }

  if (errors.length) problem(`client page errors: ${errors.join('; ')}`);
  report.hostLog = host.out.split('\n').filter((l) => /WARN|ERROR/.test(l)).slice(0, 40);
  for (const l of report.hostLog) if (/ERROR/.test(l)) problem(`host: ${l.trim()}`);
}

let failed = false;
try {
  await run();
} catch (e) {
  failed = true;
  problem(`run aborted: ${e.stack ?? e}`);
} finally {
  report.finishedAt = new Date().toISOString();
  await mkdir(dirname(resolve(opts.report)), { recursive: true });
  await writeFile(opts.report, JSON.stringify(report, null, 2));
  say(`\nreport: ${resolve(opts.report)}`);
  say(report.problems.length ? `${report.problems.length} problem(s):\n- ${report.problems.join('\n- ')}` : 'no problems found');
  for (const fn of cleanups.reverse()) await Promise.resolve(fn()).catch(() => {});
  process.exit(failed || report.problems.length ? 1 : 0);
}
