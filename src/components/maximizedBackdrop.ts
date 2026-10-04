import { fetchCachedImageUrl } from "../lib/imageCache";
import {
  admitImage,
  isImageReady,
  rememberReadyImage,
} from "../lib/imageAdmission";

/** One-time light-theme grade for the maximized player's baked bitmap.
 *  backdrop-brightness(1.6) then backdrop-saturate(0.5) would resample the
 *  whole window every frame. Brightness is applied first, matching that
 *  order. Rec. 709 weights are the ones CSS saturate() uses. */

const BRIGHTNESS = 1.6;
const SATURATION = 0.5;

function clamp8(v: number): number {
  if (v <= 0) return 0;
  if (v >= 255) return 255;
  return Math.round(v);
}

export function gradeLightBackdrop(data: Uint8ClampedArray): void {
  const keep = SATURATION;
  const gray = 1 - keep;
  for (let i = 0; i < data.length; i += 4) {
    const r = data[i] * BRIGHTNESS;
    const g = data[i + 1] * BRIGHTNESS;
    const b = data[i + 2] * BRIGHTNESS;
    const y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    data[i] = clamp8(y * gray + r * keep);
    data[i + 1] = clamp8(y * gray + g * keep);
    data[i + 2] = clamp8(y * gray + b * keep);
  }
}

/** Dark themes never used a backdrop grade. Light themes take it once. */
export function finishBakedBackdrop(
  data: Uint8ClampedArray,
  isDark: boolean,
): void {
  if (!isDark) gradeLightBackdrop(data);
}

/** Solid wash over the baked bitmap. No backdrop-filter. */
export function maximizedWashStyle(
  isDark: boolean,
  bgBaseRgb: string,
): { className: string; backgroundColor: string } {
  return {
    className: "absolute inset-0",
    backgroundColor: isDark ? "rgba(0,0,0,0.6)" : `rgba(${bgBaseRgb},0.45)`,
  };
}

// One separable box-blur pass (horizontal or vertical) over RGBA pixels, using a
// sliding running-sum so cost is O(pixels) regardless of radius. Edges clamped.
function boxBlurPass(
  data: Uint8ClampedArray,
  w: number,
  h: number,
  radius: number,
  horizontal: boolean,
) {
  const div = radius * 2 + 1;
  const lineLen = horizontal ? w : h;
  const lineCount = horizontal ? h : w;
  const stride = horizontal ? 4 : w * 4; // step between pixels along a line
  const lineStep = horizontal ? w * 4 : 4; // step between lines
  const line = new Float32Array(lineLen * 4);
  for (let l = 0; l < lineCount; l++) {
    const base = l * lineStep;
    let sr = 0,
      sg = 0,
      sb = 0,
      sa = 0;
    for (let i = -radius; i <= radius; i++) {
      const p = base + Math.min(lineLen - 1, Math.max(0, i)) * stride;
      sr += data[p];
      sg += data[p + 1];
      sb += data[p + 2];
      sa += data[p + 3];
    }
    for (let x = 0; x < lineLen; x++) {
      line[x * 4] = sr / div;
      line[x * 4 + 1] = sg / div;
      line[x * 4 + 2] = sb / div;
      line[x * 4 + 3] = sa / div;
      const pOut =
        base + Math.min(lineLen - 1, Math.max(0, x - radius)) * stride;
      const pIn =
        base + Math.min(lineLen - 1, Math.max(0, x + radius + 1)) * stride;
      sr += data[pIn] - data[pOut];
      sg += data[pIn + 1] - data[pOut + 1];
      sb += data[pIn + 2] - data[pOut + 2];
      sa += data[pIn + 3] - data[pOut + 3];
    }
    for (let x = 0; x < lineLen; x++) {
      const p = base + x * stride;
      data[p] = line[x * 4];
      data[p + 1] = line[x * 4 + 1];
      data[p + 2] = line[x * 4 + 2];
      data[p + 3] = line[x * 4 + 3];
    }
  }
}

// Dependency-free near-gaussian blur: 3 box passes per axis (central-limit
// theorem ≈ gaussian). Runs ONCE per track, never on the per-frame paint path.
function blurRGBA(
  data: Uint8ClampedArray,
  w: number,
  h: number,
  radius: number,
) {
  if (radius < 1) return;
  for (let pass = 0; pass < 3; pass++) {
    boxBlurPass(data, w, h, radius, true);
    boxBlurPass(data, w, h, radius, false);
  }
}

/** Bound the baked surface independently of display size or pixel density. */
export function backdropDimensions(width: number, height: number) {
  const scale = Math.min(1, 480 / width, 320 / height);
  return {
    width: Math.max(1, Math.round(width * scale)),
    height: Math.max(1, Math.round(height * scale)),
  };
}

/** Load through the shared cache and decoder budget, publishing only a finished
 * bitmap. Cancellation preserves the preceding track's visible background. */
export function bakeMaximizedBackdrop(
  canvas: HTMLCanvasElement,
  src: string,
  isDark: boolean,
  viewportWidth: number,
  viewportHeight: number,
): () => void {
  const ctx = canvas.getContext("2d");
  if (!ctx) return () => {};
  const { width, height } = backdropDimensions(viewportWidth, viewportHeight);
  const radius = Math.max(1, Math.round(40 * (width / viewportWidth)));
  const controller = new AbortController();
  const image = new Image();
  let release = () => {};
  const paint = () => {
    if (controller.signal.aborted) return;
    // Do not resize or clear the visible canvas until the new bake succeeds.
    const offscreen = document.createElement("canvas");
    offscreen.width = width;
    offscreen.height = height;
    const off = offscreen.getContext("2d");
    if (!off || !image.width || !image.height) return;
    const scale = Math.max(width / image.width, height / image.height) * 1.1;
    const dw = image.width * scale;
    const dh = image.height * scale;
    off.drawImage(image, (width - dw) / 2, (height - dh) / 2, dw, dh);
    const pixels = off.getImageData(0, 0, width, height);
    blurRGBA(pixels.data, width, height, radius);
    finishBakedBackdrop(pixels.data, isDark);
    off.putImageData(pixels, 0, 0);
    if (controller.signal.aborted) return;
    if (canvas.width !== width) canvas.width = width;
    if (canvas.height !== height) canvas.height = height;
    ctx.drawImage(offscreen, 0, 0);
  };
  // Download first; the admission slot covers browser image load/decode work.
  void fetchCachedImageUrl(src, { signal: controller.signal })
    .then((url) => {
      if (controller.signal.aborted) return;
      release = admitImage((done) => {
        image.onload = () => {
          void (async () => {
            try {
              await image.decode?.();
            } catch {
              // WebKit may reject decode after a successful native load.
            }
            try {
              if (!controller.signal.aborted) {
                rememberReadyImage(url);
                paint();
              }
            } catch {
              // A failed bake retains the preceding completed background.
            } finally {
              done();
            }
          })();
        };
        image.onerror = () => done();
        image.src = url;
      }, isImageReady(url));
    })
    .catch(() => {
      // Fetch failure or cancellation also preserves the preceding bitmap.
    });
  return () => {
    controller.abort();
    image.onload = null;
    image.onerror = null;
    image.removeAttribute("src");
    release();
  };
}
