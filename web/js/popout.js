// The second monitor's video in a window of its own, so each host monitor can fill a client screen.
//
// The window is empty but for the video and the host pointer's overlay. Its mouse, wheel and keys go
// to the page's InputCapture, which treats the window's video as the second monitor's: `input.video2`
// is this one while the window is open. Absolute mouse mode and pointer lock both work in it.

// Opens the window; null if the browser blocked it (pop-ups need a click, and may need allowing).
// `onClose` runs once when the window goes away, by `close()` or by the user.
export function openPopout({ input, onClose }) {
  const win = window.open('', 'ws-second-monitor', 'popup=yes,width=1280,height=720');
  if (!win) return null;
  const doc = win.document;
  doc.title = 'web-stream: second monitor';
  const page = doc.documentElement;
  Object.assign(page.style, { height: '100%', background: '#000' });
  Object.assign(doc.body.style, { margin: '0', height: '100%', background: '#000', overflow: 'hidden' });

  const view = doc.createElement('div');
  Object.assign(view.style, { position: 'relative', width: '100%', height: '100%' });
  const video = doc.createElement('video');
  video.autoplay = true;
  video.playsInline = true;
  video.disablePictureInPicture = true;
  Object.assign(video.style, { width: '100%', height: '100%', objectFit: 'contain', display: 'block', background: '#000', userSelect: 'none' });
  const overlay = doc.createElement('img');
  overlay.alt = '';
  overlay.hidden = true;
  Object.assign(overlay.style, { position: 'absolute', left: '0', top: '0', transformOrigin: '0 0', pointerEvents: 'none', imageRendering: 'pixelated' });
  view.append(video, overlay);
  doc.body.append(view);

  const opt = { signal: undefined };
  const stop = new AbortController();
  opt.signal = stop.signal;
  const mine = (e) => input.locked || e.target === video;
  doc.addEventListener('mousedown', (e) => { if (mine(e)) input.onMouseDown(e); }, opt);
  doc.addEventListener('mouseup', (e) => { if (mine(e)) input.onMouseUp(e); }, opt);
  doc.addEventListener('mousemove', (e) => { if (mine(e)) input.onMouseMove(e); }, opt);
  doc.addEventListener('wheel', (e) => { if (mine(e)) input.onWheel(e); }, { ...opt, passive: false });
  doc.addEventListener('contextmenu', (e) => e.preventDefault(), opt);
  // Like the page's own stage: only while the pointer is over it do keys go to the host.
  page.addEventListener('mouseenter', () => { input.stageFocused = true; }, opt);
  page.addEventListener('mouseleave', () => {
    if (input.mode === 'absolute') input.releaseAll();
    input.stageFocused = false;
  }, opt);
  win.addEventListener('keydown', (e) => input.onKey(e, true), { ...opt, capture: true });
  win.addEventListener('keyup', (e) => input.onKey(e, false), { ...opt, capture: true });
  win.addEventListener('blur', () => input.releaseAll(), opt);
  doc.addEventListener('pointerlockchange', () => { if (!input.locked) input.releaseAll(); }, opt);
  // Double-click: fullscreen on the screen the window is on, and back.
  doc.addEventListener('dblclick', () => {
    if (doc.fullscreenElement) doc.exitFullscreen().catch(() => {});
    else page.requestFullscreen().catch(() => {});
  }, opt);

  let closed = false;
  const close = () => {
    if (closed) return;
    closed = true;
    stop.abort();
    try { win.close(); } catch { /* already gone */ }
    onClose();
  };
  win.addEventListener('pagehide', close, { once: true });
  return { video, overlay, window: win, close };
}
