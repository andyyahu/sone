import { memo, useState, useEffect, useRef } from "react";
import { ListMusic, Play, User } from "lucide-react";
import { observeNearViewport } from "../lib/nearViewport";
import { usePageScrollElement } from "../contexts/PageScrollContext";
import ScheduledImage from "./ScheduledImage";

import { fetchCachedImageUrl, getCachedImageUrl } from "../lib/imageCache";
export { fetchCachedImageUrl } from "../lib/imageCache";

interface TidalImageProps {
  src: string | undefined;
  alt: string;
  className?: string;
  type?: "album" | "playlist" | "artist";
  objectFit?:
    | "object-cover"
    | "object-contain"
    | "object-fill"
    | "object-none"
    | "object-scale-down"
    | "object-center"
    | "object-top"
    | "object-bottom"
    | "object-left"
    | "object-right"
    | "object-top-left"
    | "object-top-right"
    | "object-bottom-left"
    | "object-bottom-right";
  onLoad?: () => void;
  /** Bypass viewport gating for a known above-the-fold cover. */
  loading?: "lazy" | "eager";
}

function TidalImageComponent({
  src,
  alt,
  className = "w-full h-full",
  type = "album",
  objectFit = "object-cover",
  onLoad,
  loading = "lazy",
}: TidalImageProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const scrollElement = usePageScrollElement();
  const [nearViewport, setNearViewport] = useState(loading === "eager");
  const [hasError, setHasError] = useState(false);
  // Synchronous cache check — if the blob is already in memory, skip loading entirely
  const [image, setImage] = useState<{ src: string; url: string } | undefined>(
    () => {
      if (!src) return undefined;
      const url = getCachedImageUrl(src);
      return url ? { src, url } : undefined;
    },
  );
  const blobUrl = image?.url;
  const [isLoading, setIsLoading] = useState(blobUrl === undefined);

  useEffect(() => {
    const element = containerRef.current;
    if (loading === "eager" || nearViewport || !element) return;
    // A cached blob paints from the first render. Observing it only flips
    // state, so every warm cover renders again as a playlist scrolls.
    if (src && getCachedImageUrl(src)) return;
    return observeNearViewport(
      element,
      () => setNearViewport(true),
      scrollElement,
    );
  }, [loading, nearViewport, scrollElement, src]);

  useEffect(() => {
    if (!src) return;

    // Sync cache hit — skip loading shimmer
    const cached = getCachedImageUrl(src);
    if (cached) {
      setImage((current) =>
        current?.src === src && current.url === cached
          ? current
          : { src, url: cached },
      );
      setIsLoading(false);
      setHasError(false);
      return;
    }

    if (loading !== "eager" && !nearViewport) return;

    // Not cached — fetch silently, keep old image visible until ready
    setHasError(false);

    let cancelled = false;
    const controller = new AbortController();
    fetchCachedImageUrl(src, { signal: controller.signal })
      .then((url) => {
        if (!cancelled) {
          setImage({ src, url });
          setHasError(false);
        }
      })
      .catch(() => {
        if (!cancelled) setHasError(true);
      });

    return () => {
      cancelled = true;
      controller.abort();
    };
  }, [src, nearViewport, loading]);

  if (!src || hasError) {
    return (
      <div
        ref={containerRef}
        className={`bg-gradient-to-br from-th-button to-th-surface flex items-center justify-center ${className}`}
      >
        {type === "playlist" ? (
          <Play size={24} className="text-gray-600" />
        ) : type === "artist" ? (
          <User size={24} className="text-gray-600" />
        ) : (
          <ListMusic size={24} className="text-gray-600" />
        )}
      </div>
    );
  }

  if (!blobUrl) {
    return (
      <div ref={containerRef} className={`relative ${className}`}>
        <div className="absolute inset-0 bg-th-surface-hover" />
      </div>
    );
  }

  return (
    <div ref={containerRef} className={`relative ${className}`}>
      {isLoading && <div className="absolute inset-0 bg-th-surface-hover" />}
      <ScheduledImage
        src={blobUrl}
        alt={alt}
        draggable={false}
        decoding="async"
        className={`w-full h-full ${isLoading ? "opacity-0" : "opacity-100"} transition-opacity ${objectFit}`}
        onError={() => {
          // A retained cover may finish after src changes. Its failure must
          // not hide the replacement while the new IPC request is pending.
          if (image?.src === src) setHasError(true);
        }}
        onLoad={() => {
          setIsLoading(false);
          if (image?.src === src) onLoad?.();
        }}
      />
    </div>
  );
}

const TidalImage = memo(TidalImageComponent);
TidalImage.displayName = "TidalImage";

export default TidalImage;

export function preloadImage(url: string): void {
  if (!url) return;
  fetchCachedImageUrl(url, { priority: "prefetch" }).catch(() => {});
}
