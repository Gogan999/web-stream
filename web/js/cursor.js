// Shows the host pointer; the video carries none. In absolute mode it is the video's CSS cursor, so
// the browser draws it with no added latency: standard shapes as the browser's own, anything else as
// the host's image. In relative mode pointer lock hides the browser's pointer, so the host's image is
// drawn over the video where the host says its pointer is; the host sends positions only while the
// browser asks for them (setCursorPositions).

const CSS_KINDS = new Set(['default', 'pointer', 'text', 'wait', 'progress', 'crosshair', 'move', 'ew-resize',
  'ns-resize', 'nwse-resize', 'nesw-resize', 'not-allowed', 'help', 'none']);
const MAX_SIDE = 128; // what the host sends at most; also Chrome's limit for CSS cursor images
// The host's id table holds as many and never reuses an id, so the newest this many are all it may
// name without resending the image.
const MAX_IMAGES = 256;

// The image of a cursorShape message as { width, height, hotX, hotY, rgba }, or null if malformed.
export function decodeImage(img) {
  if (!img || typeof img.rgba !== 'string') return null;
  const { width, height, hotX, hotY } = img;
  const side = (v) => Number.isInteger(v) && v >= 1 && v <= MAX_SIDE;
  if (!side(width) || !side(height) || !Number.isInteger(hotX) || !Number.isInteger(hotY) ||
      hotX < 0 || hotY < 0 || hotX >= width || hotY >= height) return null;
  let bin;
  try { bin = atob(img.rgba); } catch { return null; }
  if (bin.length !== width * height * 4) return null;
  const rgba = new Uint8ClampedArray(bin.length);
  for (let i = 0; i < bin.length; i++) rgba[i] = bin.charCodeAt(i);
  return { width, height, hotX, hotY, rgba };
}

// The CSS cursor for a shape; `image` is the host's ({ url, width, height, hotX, hotY }) if known.
export function cssCursor(kind, image) {
  if (kind !== 'custom' && CSS_KINDS.has(kind)) return kind;
  if (image?.url && image.width <= MAX_SIDE && image.height <= MAX_SIDE) {
    return `url("${image.url}") ${image.hotX} ${image.hotY}, default`;
  }
  return 'default';
}

// Where the overlay goes inside a letterboxed (object-fit: contain) video of size `box`, in CSS px, or
// null when the pointer is off the streamed display. `pos` is in host pixels from the display's
// top-left; the picture shows the whole display, as absolute mouse input assumes.
export function overlayBox(pos, image, display, box, videoW, videoH) {
  if (!pos || !image || !display?.width || !display?.height || !videoW || !videoH) return null;
  if (pos.x < 0 || pos.y < 0 || pos.x >= display.width || pos.y >= display.height) return null;
  const scale = Math.min(box.width / videoW, box.height / videoH);
  const w = videoW * scale;
  const h = videoH * scale;
  const sx = w / display.width;
  const sy = h / display.height;
  return {
    left: (box.width - w) / 2 + (pos.x - image.hotX) * sx,
    top: (box.height - h) / 2 + (pos.y - image.hotY) * sy,
    width: image.width * sx,
    height: image.height * sy,
  };
}

function toDataUrl({ width, height, rgba }) {
  const canvas = document.createElement('canvas');
  canvas.width = width;
  canvas.height = height;
  canvas.getContext('2d').putImageData(new ImageData(rgba, width, height), 0, 0);
  return canvas.toDataURL('image/png');
}

export class CursorView {
  constructor({ video, overlay, displaySize, onPlace = null }) {
    this.video = video;
    this.overlay = overlay; // an <img> over the video
    this.displaySize = displaySize; // () => { width, height } of the streamed display
    this.onPlace = onPlace; // (x, y) where the overlay's hotspot was drawn, in the video's parent's px
    this.images = new Map(); // id -> { url, width, height, hotX, hotY }, for this session
    this.kind = 'default';
    this.id = null;
    this.pos = null;
    this.mode = 'relative';
    this.raf = 0;
    this.resize = new ResizeObserver(() => this.schedule());
    this.resize.observe(video);
    video.addEventListener('resize', () => this.schedule()); // stream resolution changed
  }

  setMode(mode) {
    this.mode = mode;
    this.schedule();
  }

  // Handles cursorShape and cursorPosition; returns whether it was one of them.
  onControl(msg) {
    if (msg.type === 'cursorShape') {
      this.kind = typeof msg.kind === 'string' ? msg.kind : 'default';
      this.id = Number.isInteger(msg.id) ? msg.id : null;
      if (this.id != null && msg.image) this.remember(this.id, msg.image);
    } else if (msg.type === 'cursorPosition') {
      if (!Number.isFinite(msg.x) || !Number.isFinite(msg.y)) return true;
      this.pos = { x: msg.x, y: msg.y };
    } else {
      return false;
    }
    this.schedule();
    return true;
  }

  remember(id, raw) {
    const img = decodeImage(raw);
    if (!img) return;
    if (this.images.size >= MAX_IMAGES) this.images.delete(this.images.keys().next().value);
    const { width, height, hotX, hotY } = img;
    this.images.set(id, { url: toDataUrl(img), width, height, hotX, hotY });
  }

  schedule() {
    if (!this.raf) this.raf = requestAnimationFrame(() => { this.raf = 0; this.render(); });
  }

  render() {
    const image = this.id != null ? this.images.get(this.id) : null;
    this.video.style.cursor = this.mode === 'absolute' ? cssCursor(this.kind, image) : '';
    const box = this.mode === 'relative' && this.kind !== 'none' && overlayBox(this.pos, image, this.displaySize?.(),
      { width: this.video.clientWidth, height: this.video.clientHeight }, this.video.videoWidth, this.video.videoHeight);
    const o = this.overlay;
    if (!box) {
      o.hidden = true;
      return;
    }
    if (o.getAttribute('src') !== image.url) o.src = image.url;
    o.style.width = `${box.width}px`;
    o.style.height = `${box.height}px`;
    // Inside the zoomed element, so it zooms with the picture and stays in scale with it.
    const left = this.video.offsetLeft + box.left;
    const top = this.video.offsetTop + box.top;
    o.style.transform = `translate(${left}px, ${top}px)`;
    o.hidden = false;
    this.onPlace?.(left + image.hotX * box.width / image.width, top + image.hotY * box.height / image.height);
  }

  stop() {
    cancelAnimationFrame(this.raf);
    this.raf = 0;
    this.resize.disconnect();
    this.images.clear();
    this.video.style.cursor = '';
    this.overlay.hidden = true;
  }
}
