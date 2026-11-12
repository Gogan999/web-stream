import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const web = (p) => fileURLToPath(new URL(`../../${p}`, import.meta.url));
const manifest = JSON.parse(readFileSync(web('manifest.webmanifest'), 'utf8'));
const html = readFileSync(web('index.html'), 'utf8');

function pngSize(file) {
  const b = readFileSync(file);
  assert.equal(b.subarray(1, 4).toString(), 'PNG');
  return [b.readUInt32BE(16), b.readUInt32BE(20)];
}

test('every manifest icon exists at its declared size', () => {
  assert.ok(manifest.icons.some((i) => i.purpose === 'maskable'));
  for (const i of manifest.icons) {
    assert.ok(existsSync(web(`${i.src}`)), i.src);
    const [w, h] = i.sizes.split('x').map(Number);
    assert.deepEqual(pngSize(web(i.src)), [w, h], i.src);
  }
});

test('the home-screen shortcut opens connected, and iOS has its icon and metas', () => {
  assert.equal(manifest.start_url, './?autoconnect');
  assert.match(html, /<link rel="apple-touch-icon" href="icons\/apple-touch-icon\.png">/);
  assert.deepEqual(pngSize(web('icons/apple-touch-icon.png')), [180, 180]);
  assert.match(html, /apple-mobile-web-app-capable/);
  assert.match(html, /apple-mobile-web-app-title/);
});
