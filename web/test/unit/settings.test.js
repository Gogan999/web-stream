import test from 'node:test';
import assert from 'node:assert/strict';
import { autoKbps, fitToDisplay, streamSize, videoConfig, sameConfig, load, save, DEFAULTS } from '../../js/settings.js';

test('auto bitrate is 20 Mb/s at 1080p60 and grows with pixel rate', () => {
  assert.equal(autoKbps(1920, 1080, 60), 20000);
  assert.ok(autoKbps(2560, 1440, 60) > 30000 && autoKbps(2560, 1440, 60) < 40000);
  assert.ok(autoKbps(3840, 2160, 60) > 60000);
  assert.equal(autoKbps(320, 180, 30), 5000);
  assert.equal(autoKbps(7680, 4320, 240), 150000);
});

test('screen size is fitted to the display shape and never upscaled', () => {
  // 16:10 laptop screen, 16:9 host display: full width, shorter height.
  assert.deepEqual(fitToDisplay(2560, 1600, 2560, 1440), { width: 2560, height: 1440 });
  // Small screen, big display: scaled down to fit.
  assert.deepEqual(fitToDisplay(1366, 768, 3840, 2160), { width: 1364, height: 768 });
  // Big screen, small display: the display's own size.
  assert.deepEqual(fitToDisplay(3840, 2160, 1920, 1080), { width: 1920, height: 1080 });
});

test('stream size for each resolution setting', () => {
  const display = { width: 2560, height: 1440 };
  const screen = { width: 1920, height: 1200 };
  assert.deepEqual(streamSize('screen', screen, display), { width: 1920, height: 1080 });
  assert.deepEqual(streamSize('native', screen, display), { width: 2560, height: 1440 });
  assert.deepEqual(streamSize('1080', screen, display), { width: 1920, height: 1080 });
  assert.deepEqual(streamSize('2160', screen, display), { width: 2560, height: 1440 });
  // An ultrawide display keeps its shape and the height.
  assert.deepEqual(streamSize('1080', screen, { width: 3440, height: 1440 }), { width: 2580, height: 1080 });
  assert.deepEqual(streamSize('1440', screen, { width: 3440, height: 1440 }), { width: 3440, height: 1440 });
});

test('video config from settings', () => {
  const c = videoConfig({ ...DEFAULTS }, { screen: { width: 1920, height: 1080 }, display: { width: 1920, height: 1080 }, codec: 'av1' });
  assert.deepEqual(c, { codec: 'av1', width: 1920, height: 1080, fps: 60, bitrateKbps: 20000, quality: 'balanced' });
  const fixed = videoConfig({ ...DEFAULTS, bitrate: '50000', quality: 'clarity', fps: '120' }, { screen: { width: 1920, height: 1080 }, display: null });
  assert.equal(fixed.bitrateKbps, 50000);
  assert.equal(fixed.fps, 120);
  assert.equal(fixed.quality, 'clarity');
  assert.ok(sameConfig(c, { ...c }));
  // A host that doesn't send quality is on the default.
  const { quality, ...old } = c;
  assert.ok(sameConfig(c, old));
  assert.ok(!sameConfig(c, { ...c, fps: 30 }));
});

test('settings survive broken storage', () => {
  const broken = { getItem() { throw new Error('denied'); }, setItem() { throw new Error('denied'); } };
  assert.deepEqual(load(broken), { ...DEFAULTS });
  save({ ...DEFAULTS }, broken);
  const map = new Map();
  const mem = { getItem: (k) => map.get(k) ?? null, setItem: (k, v) => map.set(k, v) };
  save({ ...DEFAULTS, bitrate: '30000' }, mem);
  assert.equal(load(mem).bitrate, '30000');
  // Auto-tune picks off the list are kept.
  save({ ...DEFAULTS, bitrate: '42500' }, mem);
  assert.equal(load(mem).bitrate, '42500');
});

test('junk in storage falls back field by field', () => {
  const map = new Map([['ws-stream-settings', JSON.stringify({ bitrate: 'abc', resolution: 'foo', fps: 59.5, quality: 'max', matchHost: 'yes' })]]);
  const mem = { getItem: (k) => map.get(k) ?? null, setItem: (k, v) => map.set(k, v) };
  assert.deepEqual(load(mem), { ...DEFAULTS });
  map.set('ws-stream-settings', JSON.stringify({ bitrate: '999999', fps: '120', quality: 'clarity' }));
  assert.deepEqual(load(mem), { ...DEFAULTS, fps: 120, quality: 'clarity' });
  map.set('ws-stream-settings', '[1,2]');
  assert.deepEqual(load(mem), { ...DEFAULTS });
  const c = videoConfig({ bitrate: 'abc', resolution: 'foo', fps: 59.5 }, { screen: { width: 1920, height: 1080 }, display: { width: 1920, height: 1080 } });
  assert.deepEqual([c.width, c.height, c.fps, c.bitrateKbps], [1920, 1080, 60, 20000]);
});
