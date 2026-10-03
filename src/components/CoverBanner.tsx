import { useEffect, useRef, useState } from "react";
import { fetchCachedImageUrl } from "../lib/imageCache";
import {
  admitImage,
  isImageReady,
  rememberReadyImage,
} from "../lib/imageAdmission";
import TidalImage from "./TidalImage";

interface CoverBannerProps {
  /** Resolved image URL (UUID already run through getTidalImageUrl, or a direct URL). */
  src: string | undefined;
  /**
   * "blur" — heavily blurred, faint backdrop (playlists, mixes).
   * "dark" — sharp art, darkened, TIDAL album-page style.
   */
  variant?: "blur" | "dark";
}

// Fade applied to the unscaled outer box so it dissolves at the true bottom edge.
const FADE = "linear-gradient(to bottom, #000 0%, #000 50%, transparent 100%)";
// Sharp art (dark variant) is more present, so start dissolving sooner.
const DARK_FADE =
  "linear-gradient(to bottom, #000 0%, #000 22%, transparent 100%)";

// The old CSS blur(48px) was rasterized at the banner's on-screen size and
// stayed live for the whole scroll. A fullscreen window makes that layer big
// enough to drop frames. Bake the same softness into a capped bitmap once.
const BLUR_CSS_PX = 48;
const BAKED_MAX_WIDTH = 480;
const BAKED_MAX_HEIGHT = 320;

function canvasForBlur(
  image: HTMLImageElement,
  displayWidth: number,
  displayHeight: number,
): HTMLCanvasElement | null {
  if (image.naturalWidth < 1 || image.naturalHeight < 1) return null;
  const canvas = document.createElement("canvas");
  canvas.width = Math.max(
    1,
    Math.min(BAKED_MAX_WIDTH, Math.round(displayWidth)),
  );
  canvas.height = Math.max(
    1,
    Math.min(
      BAKED_MAX_HEIGHT,
      Math.round(canvas.width * (displayHeight / displayWidth)),
    ),
  );
  const ctx = canvas.getContext("2d");
  if (!ctx) return null;
  const blurPx = Math.max((BLUR_CSS_PX * canvas.width) / displayWidth, 0.5);
  const scale = canvas.width / image.naturalWidth;
  const width = image.naturalWidth * scale;
  const height = image.naturalHeight * scale;
  const pad = Math.ceil(blurPx * 3);
  const draw = () => {
    ctx.drawImage(image, -pad, -pad, width + pad * 2, height + pad * 2);
  };
  try {
    ctx.filter = `blur(${blurPx}px) saturate(1.2)`;
    draw();
  } catch {
    ctx.filter = "none";
    draw();
  }
  return canvas;
}

function useBakedCoverBlur(src: string) {
  const hostRef = useRef<HTMLDivElement>(null);
  const [baked, setBaked] = useState<{ src: string; url: string } | null>(null);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    const controller = new AbortController();
    let cancelled = false;
    let decoded: HTMLImageElement | null = null;
    let sourceUrl: string | null = null;
    let currentUrl: string | null = null;
    let ownsCurrent = false;
    let lastWidth = -1;
    let generation = 0;
    let cancelAdmission = () => {};

    const publish = (url: string, owned: boolean) => {
      const previous = ownsCurrent ? currentUrl : null;
      currentUrl = url;
      ownsCurrent = owned;
      setBaked({ src, url });
      if (previous) URL.revokeObjectURL(previous);
    };

    const paint = () => {
      if (cancelled || !decoded) return;
      const displayWidth = host.clientWidth;
      const displayHeight = host.clientHeight;
      if (displayWidth < 2 || displayHeight < 2) return;
      if (currentUrl && Math.abs(displayWidth - lastWidth) < 32) return;
      const canvas = canvasForBlur(decoded, displayWidth, displayHeight);
      if (!canvas || typeof canvas.toBlob !== "function") {
        // Still a plain image, never a live window-sized filter. The source
        // blob belongs to the image cache and must not be revoked here.
        if (sourceUrl && !currentUrl) publish(sourceUrl, false);
        return;
      }
      const ticket = ++generation;
      canvas.toBlob((blob) => {
        if (cancelled || !blob || ticket !== generation) return;
        lastWidth = displayWidth;
        publish(URL.createObjectURL(blob), true);
      }, "image/jpeg");
    };

    fetchCachedImageUrl(src, { signal: controller.signal })
      .then((objectUrl) => {
        if (cancelled) return;
        sourceUrl = objectUrl;
        cancelAdmission = admitImage((done) => {
          const image = new Image();
          image.onload = () => {
            try {
              if (!cancelled) {
                decoded = image;
                rememberReadyImage(objectUrl);
                paint();
              }
            } finally {
              done();
            }
          };
          image.onerror = () => done();
          image.src = objectUrl;
        }, isImageReady(objectUrl));
      })
      .catch(() => {});

    const observer =
      typeof ResizeObserver === "undefined"
        ? null
        : new ResizeObserver(() => paint());
    observer?.observe(host);

    return () => {
      cancelled = true;
      controller.abort();
      cancelAdmission();
      observer?.disconnect();
      if (ownsCurrent && currentUrl) URL.revokeObjectURL(currentUrl);
    };
  }, [src]);

  return { hostRef, url: baked?.src === src ? baked.url : null };
}

function BlurredCover({ src }: { src: string }) {
  const { hostRef, url } = useBakedCoverBlur(src);
  return (
    <div ref={hostRef} className="absolute inset-0">
      {url && (
        <img
          src={url}
          alt=""
          draggable={false}
          data-cover-blur="baked"
          className="absolute inset-0 h-full w-full"
        />
      )}
    </div>
  );
}

/**
 * Album/playlist/mix art rendered as a banner behind the page header. The crisp
 * cover keeps living in its own box on top of this. Renders nothing when there's
 * no artwork.
 */
export default function CoverBanner({
  src,
  variant = "blur",
}: CoverBannerProps) {
  if (!src) return null;

  const fade = variant === "dark" ? DARK_FADE : FADE;

  return (
    <div
      className="pointer-events-none absolute inset-0 overflow-hidden select-none"
      style={{ maskImage: fade, WebkitMaskImage: fade }}
    >
      {variant === "blur" ? (
        <BlurredCover src={src} />
      ) : (
        <div className="absolute inset-0">
          <TidalImage
            src={src}
            alt=""
            className="w-full h-full"
            objectFit="object-cover"
          />
        </div>
      )}
      {/* Semitransparent base-tone layer: darkens on dark themes, lightens on
          light themes, so the bright blur reads as TIDAL's deep tone either way. */}
      <div className="absolute inset-0 bg-th-base/50" />
      {variant === "blur" ? (
        <div className="absolute inset-0 bg-gradient-to-r from-transparent via-th-base/10 to-th-base/60" />
      ) : (
        <div className="absolute inset-0 bg-gradient-to-r from-th-base/30 via-th-base/70 to-transparent" />
      )}
    </div>
  );
}
