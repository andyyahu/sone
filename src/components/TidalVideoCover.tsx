import { memo, useEffect, useRef, useState } from "react";
import { useAtomValue } from "jotai";
import { videoCoversAtom } from "../atoms/ui";
import { getTidalImageUrl, getTidalVideoUrl } from "../types";
import TidalImage from "./TidalImage";
import { useDocumentVisible } from "../hooks/useDocumentVisible";

interface TidalVideoCoverProps {
  cover?: string;
  videoCover?: string;
  // Video resolution: 640 / 1280 bracket, or "origin" (native).
  size: number | "origin";
  // Poster / static-fallback image size (defaults to `size`, or 1280 for "origin").
  imageSize?: number;
  alt: string;
  className?: string;
  /** False when an enclosing player is closed or covered by another overlay. */
  active?: boolean;
}

function AnimatedCover({
  videoUrl,
  poster,
  className,
  active,
}: {
  videoUrl: string;
  poster: string;
  className: string;
  active: boolean;
}) {
  const documentVisible = useDocumentVisible();
  const videoRef = useRef<HTMLVideoElement>(null);
  const [inViewport, setInViewport] = useState(
    () => typeof IntersectionObserver === "undefined",
  );

  useEffect(() => {
    const video = videoRef.current;
    if (!video || typeof IntersectionObserver === "undefined") return;
    const observer = new IntersectionObserver(([entry]) => {
      setInViewport(entry.isIntersecting);
    });
    observer.observe(video);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    const video = videoRef.current;
    if (!video || !videoUrl) return;
    if (active && documentVisible && inViewport) {
      // A closed drawer must not start a download on mount. Keep the source
      // after first use so pause/resume preserves the cover's playback time.
      if (video.getAttribute("src") !== videoUrl) video.src = videoUrl;
      video.play().catch(() => {});
    } else {
      video.pause();
    }
    return () => video.pause();
  }, [active, documentVisible, inViewport, videoUrl]);

  return (
    <div className={`relative ${className}`}>
      <video
        ref={videoRef}
        poster={poster}
        preload="none"
        loop
        muted
        playsInline
        className="absolute inset-0 w-full h-full object-cover"
      />
    </div>
  );
}

function TidalVideoCoverComponent({
  cover,
  videoCover,
  size,
  imageSize,
  alt,
  className = "",
  active = true,
}: TidalVideoCoverProps) {
  const enabled = useAtomValue(videoCoversAtom);
  const videoUrl = getTidalVideoUrl(videoCover, size);
  const posterSize = imageSize ?? (typeof size === "number" ? size : 1280);

  // No animated cover (disabled or none): just the cached static art.
  if (!enabled || !videoUrl) {
    return (
      <div className={`relative ${className}`}>
        <TidalImage
          src={getTidalImageUrl(cover, posterSize)}
          alt={alt}
          className="w-full h-full"
        />
      </div>
    );
  }

  // Play the high-res video directly; the cover image is the poster, shown
  // until the first frame paints (and if playback fails). Keyed by src so a
  // track change remounts cleanly.
  return (
    <AnimatedCover
      key={videoUrl}
      videoUrl={videoUrl}
      poster={getTidalImageUrl(cover, posterSize)}
      className={className}
      active={active}
    />
  );
}

const TidalVideoCover = memo(TidalVideoCoverComponent);
TidalVideoCover.displayName = "TidalVideoCover";
export default TidalVideoCover;
