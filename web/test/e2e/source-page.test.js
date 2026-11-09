// The hardware test's source page (test/hardware/source.html) draws a stamp the probe can read
// back at any window size, shows the display it was told to, and records input events.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { chromium } from 'playwright';

test('source page: the stamp reads back at several sizes, and events are recorded', { timeout: 60000 }, async (t) => {
  const browser = await chromium.launch();
  t.after(() => browser.close());
  const stampJs = await readFile(new URL('../lib/stamp.js', import.meta.url), 'utf8');
  for (const [w, h, display] of [[1920, 1080, 0], [1280, 720, 1], [2560, 1440, 2], [1366, 768, 3]]) {
    const page = await browser.newPage({ viewport: { width: w, height: h } });
    const errors = [];
    page.on('pageerror', (e) => errors.push(e.message));
    await page.goto(new URL(`../hardware/source.html?display=${display}`, import.meta.url).href);
    await page.waitForFunction(() => window.__frames > 5);
    // Read it the way the probe does: one pixel per cell, from the middle of the cell.
    const read = await page.evaluate((js) => {
      new Function(js)();
      const c = document.getElementById('c');
      const cell = Math.floor(c.width / wsStamp.COLS);
      const data = c.getContext('2d').getImageData(0, 0, c.width, 2 * cell).data;
      const px = new Uint8ClampedArray(wsStamp.COLS * 2 * 4);
      for (let r = 0; r < 2; r++) for (let col = 0; col < wsStamp.COLS; col++) {
        const i = ((r * cell + (cell >> 1)) * c.width + col * cell + (cell >> 1)) * 4;
        px.set(data.subarray(i, i + 4), (r * wsStamp.COLS + col) * 4);
      }
      return { stamp: wsStamp.decodeRgba(px), age: wsStamp.age(wsStamp.decodeRgba(px)?.time ?? 0, Date.now()) };
    }, stampJs);
    assert.ok(read.stamp, `${w}x${h}: stamp decodes`);
    assert.equal(read.stamp.display, display);
    assert.ok(Math.abs(read.age) < 2000, `${w}x${h}: stamp time is now (${read.age} ms)`);

    await page.mouse.move(100, 100);
    await page.mouse.move(300, 200, { steps: 5 });
    await page.mouse.down(); await page.mouse.up();
    await page.keyboard.press('q');
    const ev = await page.evaluate(() => ({ events: window.__events.map((e) => e.type + (e.code ? ':' + e.code : '')), moves: window.__lastMove?.moves }));
    assert.ok(ev.moves >= 5, `${w}x${h}: mouse moves seen`);
    assert.ok(ev.events.includes('mousedown') && ev.events.includes('keydown:KeyQ'), JSON.stringify(ev));
    assert.deepEqual(errors, []);
    await page.close();
  }
});
