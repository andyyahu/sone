import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.hoisted(() => vi.fn());
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

let fetchImage: typeof import("./imageCache").fetchCachedImageUrl;
let sequence = 0;
const pending = new Map<string, (bytes: ArrayBuffer) => void>();

async function release(src: string) {
  const resolve = pending.get(src);
  pending.delete(src);
  resolve?.(new ArrayBuffer(8));
  // Drain the cache, subscriber and scheduler promise continuations.
  for (let i = 0; i < 8; i++) await Promise.resolve();
}

async function drain() {
  while (pending.size > 0) {
    for (const src of [...pending.keys()]) await release(src);
  }
}

function occupySlots() {
  return Array.from({ length: 6 }, (_, i) => fetchImage(`active-${i}`));
}

beforeEach(async () => {
  vi.resetModules();
  pending.clear();
  invoke
    .mockReset()
    .mockImplementation(
      (_command, { url }: { url: string }) =>
        new Promise<ArrayBuffer>((resolve) => pending.set(url, resolve)),
    );
  vi.stubGlobal(
    "URL",
    class extends URL {
      static createObjectURL = vi.fn(() => `blob:queued-${++sequence}`);
      static revokeObjectURL = vi.fn();
    },
  );
  fetchImage = (await import("./imageCache")).fetchCachedImageUrl;
});

afterEach(async () => {
  await drain();
  vi.unstubAllGlobals();
});

describe("image scheduling", () => {
  it("drops a departed page's queued images before loading the new visible cover", async () => {
    const blockers = occupySlots();
    const previousPage = new AbortController();
    const abandoned = Array.from({ length: 100 }, (_, i) =>
      fetchImage(`old-page-${i}`, { signal: previousPage.signal }).catch(
        (error: DOMException) => error.name,
      ),
    );
    previousPage.abort();
    const newCover = fetchImage("new-visible-cover");

    expect(await Promise.all(abandoned)).toEqual(Array(100).fill("AbortError"));
    expect(invoke).toHaveBeenCalledTimes(6);
    await release("active-0");
    expect(invoke).toHaveBeenLastCalledWith("get_image_bytes", {
      url: "new-visible-cover",
    });
    await drain();
    await Promise.all([...blockers, newCover]);
    expect(invoke).toHaveBeenCalledTimes(7);
  });

  it("keeps a deduplicated request while any visible consumer still needs it", async () => {
    occupySlots();
    const first = new AbortController();
    const second = new AbortController();
    const cancelled = fetchImage("shared", { signal: first.signal }).catch(
      (error: DOMException) => error.name,
    );
    const needed = fetchImage("shared", { signal: second.signal });
    first.abort();
    expect(await cancelled).toBe("AbortError");
    await release("active-0");
    expect(invoke).toHaveBeenLastCalledWith("get_image_bytes", {
      url: "shared",
    });
    await drain();
    await expect(needed).resolves.toMatch(/^blob:/);
    expect(
      invoke.mock.calls.filter(([, args]) => args.url === "shared"),
    ).toHaveLength(1);
  });

  it("can request a URL again after its last queued consumer cancelled", async () => {
    occupySlots();
    const controller = new AbortController();
    const abandoned = fetchImage("retry", { signal: controller.signal }).catch(
      () => null,
    );
    controller.abort();
    const retry = fetchImage("retry");
    await abandoned;
    await drain();
    await expect(retry).resolves.toMatch(/^blob:/);
    expect(
      invoke.mock.calls.filter(([, args]) => args.url === "retry"),
    ).toHaveLength(1);
  });

  it("promotes visible covers ahead of speculative prefetches without duplicate work", async () => {
    occupySlots();
    const prefetchA = fetchImage("prefetch-a", { priority: "prefetch" });
    const prefetchB = fetchImage("prefetch-b", { priority: "prefetch" });
    const nowVisible = fetchImage("prefetch-b");
    const newVisible = fetchImage("visible-c");
    expect(nowVisible).toBe(prefetchB);
    await release("active-0");
    expect(invoke).toHaveBeenLastCalledWith("get_image_bytes", {
      url: "prefetch-b",
    });
    await release("active-1");
    expect(invoke).toHaveBeenLastCalledWith("get_image_bytes", {
      url: "visible-c",
    });
    await release("active-2");
    expect(invoke).toHaveBeenLastCalledWith("get_image_bytes", {
      url: "prefetch-a",
    });
    await drain();
    await Promise.all([prefetchA, prefetchB, newVisible]);
  });

  it("does not cancel an independent preload when a component unmounts", async () => {
    occupySlots();
    const preload = fetchImage("preloaded", { priority: "prefetch" });
    const controller = new AbortController();
    const component = fetchImage("preloaded", {
      signal: controller.signal,
    }).catch(() => null);
    controller.abort();
    await component;
    await drain();
    await expect(preload).resolves.toMatch(/^blob:/);
  });

  it("lets a running request finish into cache after its consumer leaves", async () => {
    const controller = new AbortController();
    const abandoned = fetchImage("running", {
      signal: controller.signal,
    }).catch((error: DOMException) => error.name);
    controller.abort();
    expect(await abandoned).toBe("AbortError");
    const replacement = fetchImage("running");
    await drain();
    const cached = await replacement;
    expect(await fetchImage("running")).toBe(cached);
    expect(invoke).toHaveBeenCalledTimes(1);
  });

  it("releases slots even when the bridge throws synchronously", async () => {
    invoke.mockImplementationOnce(() => {
      throw new Error("Bridge closed");
    });
    const failed = fetchImage("broken").catch(() => null);
    const subsequent = Array.from({ length: 8 }, (_, i) =>
      fetchImage(`working-${i}`),
    );
    await failed;
    await drain();
    expect((await Promise.all(subsequent)).every(Boolean)).toBe(true);
    expect(invoke).toHaveBeenCalledTimes(9);
  });
});
