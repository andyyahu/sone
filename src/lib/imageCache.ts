import { invoke } from "@tauri-apps/api/core";

const MAX_BLOB_BYTES = 200 * 1024 * 1024;
const MAX_IMAGE_REQUESTS = 6;
type Priority = "visible" | "prefetch";

interface BlobEntry {
  url: string;
  size: number;
}

interface ImageRequest {
  src: string;
  priority: Priority;
  started: boolean;
  persistent: boolean;
  consumers: Set<symbol>;
  promise: Promise<string>;
  resolve: (url: string) => void;
  reject: (error: unknown) => void;
}

// Map insertion order provides LRU eviction without sorting every cached image.
const blobs = new Map<string, BlobEntry>();
let blobBytes = 0;
const inflight = new Map<string, ImageRequest>();
const queues: Record<Priority, Map<string, ImageRequest>> = {
  visible: new Map(),
  prefetch: new Map(),
};
let activeRequests = 0;

export function getCachedImageUrl(src: string): string | undefined {
  const entry = blobs.get(src);
  if (!entry) return undefined;
  blobs.delete(src);
  blobs.set(src, entry);
  return entry.url;
}

function cacheImage(src: string, buffer: ArrayBuffer): string {
  const size = buffer.byteLength;
  if (blobBytes + size > MAX_BLOB_BYTES) {
    const target = MAX_BLOB_BYTES * 0.9;
    for (const [key, entry] of blobs) {
      if (blobBytes + size <= target) break;
      URL.revokeObjectURL(entry.url);
      blobBytes -= entry.size;
      blobs.delete(key);
    }
  }
  const url = URL.createObjectURL(new Blob([buffer], { type: "image/jpeg" }));
  blobs.set(src, { url, size });
  blobBytes += size;
  return url;
}

function pumpQueue() {
  while (activeRequests < MAX_IMAGE_REQUESTS) {
    const request =
      queues.visible.values().next().value ??
      queues.prefetch.values().next().value;
    if (!request) return;
    queues[request.priority].delete(request.src);
    request.started = true;
    activeRequests++;
    // Catch synchronous bridge errors too, so a failed invocation never
    // permanently consumes one of the six slots.
    const load = async () => {
      const buffer = await invoke<ArrayBuffer>("get_image_bytes", {
        url: request.src,
      });
      return cacheImage(request.src, buffer);
    };
    void load()
      .then(request.resolve, request.reject)
      .finally(() => {
        if (inflight.get(request.src) === request) inflight.delete(request.src);
        activeRequests--;
        pumpQueue();
      });
  }
}

const abortError = () =>
  new DOMException("Image request cancelled", "AbortError");

function subscribe(
  request: ImageRequest,
  signal: AbortSignal,
): Promise<string> {
  const consumer = Symbol();
  request.consumers.add(consumer);
  return new Promise((resolve, reject) => {
    const detach = () => {
      request.consumers.delete(consumer);
      signal.removeEventListener("abort", onAbort);
    };
    const onAbort = () => {
      detach();
      reject(abortError());
      // A shared request is cancelled only after its last interested consumer
      // leaves. Already-running Tauri invocations cannot be aborted; let them
      // finish into the cache without updating an unmounted component.
      if (
        !request.started &&
        !request.persistent &&
        request.consumers.size === 0
      ) {
        queues[request.priority].delete(request.src);
        if (inflight.get(request.src) === request) inflight.delete(request.src);
        request.reject(abortError());
      }
    };
    signal.addEventListener("abort", onAbort, { once: true });
    request.promise.then(
      (url) => {
        detach();
        resolve(url);
      },
      (error: unknown) => {
        detach();
        reject(error);
      },
    );
  });
}

export function fetchCachedImageUrl(
  src: string,
  options: { signal?: AbortSignal; priority?: Priority } = {},
): Promise<string> {
  if (options.signal?.aborted) return Promise.reject(abortError());
  const cached = getCachedImageUrl(src);
  if (cached) return Promise.resolve(cached);

  const priority = options.priority ?? "visible";
  let request = inflight.get(src);
  if (!request) {
    let resolve!: ImageRequest["resolve"];
    let reject!: ImageRequest["reject"];
    const promise = new Promise<string>((accept, decline) => {
      resolve = accept;
      reject = decline;
    });
    request = {
      src,
      priority,
      promise,
      resolve,
      reject,
      started: false,
      persistent: false,
      consumers: new Set(),
    };
    inflight.set(src, request);
    queues[priority].set(src, request);
  } else if (
    !request.started &&
    priority === "visible" &&
    request.priority === "prefetch"
  ) {
    // A visible cover overtakes speculative preloads of other images.
    queues.prefetch.delete(src);
    request.priority = "visible";
    queues.visible.set(src, request);
  }

  let promise: Promise<string>;
  if (options.signal) promise = subscribe(request, options.signal);
  else {
    // Imperative hero/preload callers keep their request alive independently
    // of mounted image components and retain the shared-promise API.
    request.persistent = true;
    promise = request.promise;
  }
  pumpQueue();
  return promise;
}
