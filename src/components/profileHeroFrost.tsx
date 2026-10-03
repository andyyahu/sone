import { useEffect, useRef, useState } from "react";

/** Tailwind `blur-3xl` is 64px at the hero's on-screen size. The radius is
 *  scaled onto a capped bitmap so the frost is computed once. */
const FROST_CSS_PX = 64;
const BAKED_MAX_WIDTH = 480;
const BAKED_MAX_HEIGHT = 320;

export const PROFILE_FROST_MASK =
  "linear-gradient(to bottom, transparent 0%, transparent 35%, #000 80%)";

/** Painted class for the baked frost. A CSS blur here would be reapplied for
 *  the whole time the hero stays on screen. */
export function profileFrostClass(): string {
  return "absolute inset-0 h-full w-full object-cover";
}

export function bakeProfileFrost(
  image: CanvasImageSource,
  displayWidth: number,
  displayHeight: number,
): HTMLCanvasElement | null {
  if (displayWidth < 2 || displayHeight < 2) return null;
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
  const blurPx = Math.max((FROST_CSS_PX * canvas.width) / displayWidth, 0.5);
  const dw = canvas.width * 1.1;
  const dh = canvas.height * 1.1;
  const draw = () => {
    ctx.drawImage(
      image,
      (canvas.width - dw) / 2,
      (canvas.height - dh) / 2,
      dw,
      dh,
    );
  };
  try {
    ctx.filter = `blur(${blurPx}px)`;
    draw();
  } catch {
    ctx.filter = "none";
    draw();
  }
  ctx.filter = "none";
  return canvas;
}

export function ProfileHeroFrost({ src }: { src: string }) {
  const hostRef = useRef<HTMLDivElement>(null);
  const [url, setUrl] = useState<string | null>(null);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    let cancelled = false;
    let bakedUrl: string | null = null;
    let frame = 0;

    const image = new Image();
    image.onload = () => {
      frame = requestAnimationFrame(() => {
        if (cancelled) return;
        const canvas = bakeProfileFrost(
          image,
          host.clientWidth,
          host.clientHeight,
        );
        if (!canvas || typeof canvas.toBlob !== "function") return;
        canvas.toBlob((blob) => {
          if (cancelled || !blob) return;
          if (bakedUrl) URL.revokeObjectURL(bakedUrl);
          bakedUrl = URL.createObjectURL(blob);
          setUrl(bakedUrl);
        }, "image/jpeg");
      });
    };
    image.src = src;

    return () => {
      cancelled = true;
      if (frame) cancelAnimationFrame(frame);
      if (bakedUrl) URL.revokeObjectURL(bakedUrl);
    };
  }, [src]);

  return (
    <div
      ref={hostRef}
      aria-hidden
      className="absolute inset-0 overflow-hidden pointer-events-none"
      style={{
        maskImage: PROFILE_FROST_MASK,
        WebkitMaskImage: PROFILE_FROST_MASK,
      }}
    >
      {url && (
        <img
          src={url}
          alt=""
          draggable={false}
          data-hero-frost="baked"
          className={profileFrostClass()}
        />
      )}
    </div>
  );
}
