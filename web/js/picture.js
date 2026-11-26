// Geometry of the host display inside the video frame, shared by input mapping and the cursor
// overlay.

// Where the host display sits inside the video frame, in video pixels.
export function pictureRect(videoW, videoH, display) {
  const full = { x: 0, y: 0, width: videoW, height: videoH };
  if (!display?.width || !display?.height || !videoW || !videoH) return full;
  const even = (v) => Math.floor(v) & ~1;
  let w = videoW;
  let h = videoH;
  if (display.width * videoH > display.height * videoW) h = Math.max(2, Math.floor(display.height * videoW / display.width));
  else w = Math.max(2, Math.floor(display.width * videoH / display.height));
  w = even(w);
  h = even(h);
  return { x: even((videoW - w) / 2), y: even((videoH - h) / 2), width: w, height: h };
}
