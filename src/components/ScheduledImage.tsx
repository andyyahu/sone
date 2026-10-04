import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ImgHTMLAttributes,
  type SyntheticEvent,
} from "react";
import {
  admitImage,
  isImageReady,
  rememberReadyImage,
} from "../lib/imageAdmission";
import { observeNearViewport } from "../lib/nearViewport";
import { usePageScrollElement } from "../contexts/PageScrollContext";

interface Load {
  src: string;
  started: boolean;
  handled: boolean;
  finished: boolean;
  cancelled: boolean;
  release: () => void;
}

/** Preserve native URL loading while spreading new image work across frames. */
export default function ScheduledImage({
  src,
  loading = "eager",
  onLoad,
  onError,
  ...props
}: Omit<ImgHTMLAttributes<HTMLImageElement>, "srcSet">) {
  const imageRef = useRef<HTMLImageElement>(null);
  const scrollElement = usePageScrollElement();
  const requestRef = useRef<Load | null>(null);
  const [nearViewport, setNearViewport] = useState(loading !== "lazy");
  const ready = !!src && isImageReady(src);
  const eligible = loading !== "lazy" || nearViewport || ready;

  useEffect(() => {
    const image = imageRef.current;
    if (loading !== "lazy" || nearViewport || ready || !image) return;
    return observeNearViewport(
      image,
      () => setNearViewport(true),
      scrollElement,
    );
  }, [loading, nearViewport, ready, scrollElement]);

  useLayoutEffect(() => {
    const image = imageRef.current;
    if (!image) return;
    if (!src) {
      image.removeAttribute("src");
      return;
    }
    if (!eligible) return;

    const request: Load = {
      src,
      started: false,
      handled: false,
      finished: false,
      cancelled: false,
      release: () => {},
    };
    requestRef.current = request;
    request.release = admitImage((release) => {
      request.release = release;
      request.started = true;
      // This effect owns src for the entire request lifetime. Keeping it out
      // of JSX prevents React's batched state from losing a source restored
      // after cancellation (including StrictMode's effect replay).
      image.setAttribute("src", src);
    }, isImageReady(src));

    return () => {
      request.cancelled = true;
      request.release();
      if (requestRef.current === request) requestRef.current = null;
      // Cancel superseded in-flight native loads before another request takes
      // their slot. Keep completed artwork visible while its replacement waits.
      if (
        request.started &&
        !request.finished &&
        image.getAttribute("src") === src
      ) {
        image.removeAttribute("src");
      }
    };
  }, [src, eligible]);

  const handleLoad = (event: SyntheticEvent<HTMLImageElement>) => {
    const request = requestRef.current;
    const image = event.currentTarget;
    if (
      !request?.started ||
      request.handled ||
      request.finished ||
      image.getAttribute("src") !== request.src
    )
      return;
    request.handled = true;
    void (async () => {
      try {
        await image.decode?.();
      } catch {
        // Some WebKit image formats can load successfully but reject decode().
        // The native load event still succeeds, and must never strand a slot.
      }
      if (request.cancelled || request.finished) return;
      request.finished = true;
      rememberReadyImage(request.src);
      request.release();
    })();
    // Forward synchronously while currentTarget is valid. Decode cleanup runs
    // independently, including when a consumer's event handler throws.
    onLoad?.(event);
  };

  return (
    <img
      {...props}
      ref={imageRef}
      // Visibility is handled above. Native lazy deferral after admission could
      // leave all four slots occupied by images waiting to enter the viewport.
      loading="eager"
      decoding="async"
      onLoad={handleLoad}
      onError={(event) => {
        const request = requestRef.current;
        if (
          !request?.started ||
          request.finished ||
          event.currentTarget.getAttribute("src") !== request.src
        )
          return;
        request.finished = true;
        request.release();
        onError?.(event);
      }}
    />
  );
}
