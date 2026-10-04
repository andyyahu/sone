import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { StrictMode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { rememberReadyImage } from "../lib/imageAdmission";
import { PageScrollProvider } from "../contexts/PageScrollContext";
import ScheduledImage from "./ScheduledImage";

let frames: Map<number, FrameRequestCallback>;
let sequence = 0;
const observers: Array<{
  targets: Set<Element>;
  options: IntersectionObserverInit;
  reveal: () => void;
}> = [];
const originalDecode = Object.getOwnPropertyDescriptor(
  HTMLImageElement.prototype,
  "decode",
);
const decode = vi.fn<() => Promise<void>>();

async function nextFrame() {
  await act(async () => {
    const callbacks = [...frames.values()];
    frames.clear();
    callbacks.forEach((callback) => callback(sequence));
  });
}

beforeEach(() => {
  frames = new Map();
  observers.length = 0;
  decode.mockReset().mockResolvedValue(undefined);
  Object.defineProperty(HTMLImageElement.prototype, "decode", {
    configurable: true,
    value: decode,
  });
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    frames.set(++sequence, callback);
    return sequence;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => frames.delete(id));
  vi.stubGlobal(
    "IntersectionObserver",
    class {
      targets = new Set<Element>();
      constructor(
        private callback: IntersectionObserverCallback,
        readonly options: IntersectionObserverInit,
      ) {
        observers.push(this);
      }
      observe(target: Element) {
        this.targets.add(target);
      }
      unobserve(target: Element) {
        this.targets.delete(target);
      }
      disconnect() {
        this.targets.clear();
      }
      reveal() {
        this.callback(
          [...this.targets].map(
            (target) =>
              ({ target, isIntersecting: true }) as IntersectionObserverEntry,
          ),
          this as unknown as IntersectionObserver,
        );
      }
    },
  );
});

afterEach(async () => {
  cleanup();
  while (frames.size) await nextFrame();
  if (originalDecode)
    Object.defineProperty(HTMLImageElement.prototype, "decode", originalDecode);
  else Reflect.deleteProperty(HTMLImageElement.prototype, "decode");
  vi.unstubAllGlobals();
});

describe("scheduled cover loading", () => {
  it("keeps four load/decode slots occupied through deferred decoding", async () => {
    const finish: Array<() => void> = [];
    decode.mockImplementation(
      () => new Promise((resolve) => finish.push(resolve)),
    );
    const onLoad = vi.fn((event) => {
      expect(event.currentTarget).toBeInstanceOf(HTMLImageElement);
    });
    const { container } = render(
      <>
        {Array.from({ length: 8 }, (_, i) => (
          <ScheduledImage key={i} src={`decode-${i}`} onLoad={onLoad} />
        ))}
      </>,
    );
    await nextFrame();
    expect(container.querySelectorAll("img[src]")).toHaveLength(2);
    await nextFrame();
    expect(container.querySelectorAll("img[src]")).toHaveLength(4);
    await act(async () => {
      container.querySelectorAll("img[src]").forEach(fireEvent.load);
    });
    await nextFrame();
    expect(onLoad).toHaveBeenCalledTimes(4);
    expect(container.querySelectorAll("img[src]")).toHaveLength(4);
    await act(async () => finish[0]());
    await nextFrame();
    expect(container.querySelectorAll("img[src]")).toHaveLength(5);
    await act(async () => finish.slice(1).forEach((done) => done()));
  });

  it("shares viewport observation and preserves native URLs after admission", async () => {
    const { container } = render(
      <>
        <ScheduledImage src="https://cdn.test/lazy-a.jpg" loading="lazy" />
        <ScheduledImage src="https://cdn.test/lazy-b.jpg" loading="lazy" />
      </>,
    );
    await nextFrame();
    expect(container.querySelectorAll("img[src]")).toHaveLength(0);
    expect(observers).toHaveLength(1);
    await act(async () => observers[0].reveal());
    await nextFrame();
    expect(container.querySelector("img")?.getAttribute("src")).toBe(
      "https://cdn.test/lazy-a.jpg",
    );
    expect(container.querySelector("img")?.getAttribute("loading")).toBe(
      "eager",
    );
  });

  it("restores ready artwork under StrictMode while four cold decodes are pending", async () => {
    rememberReadyImage("return-under-cold");
    const finish: Array<() => void> = [];
    decode.mockImplementation(
      () => new Promise((resolve) => finish.push(resolve)),
    );
    const covers = Array.from({ length: 8 }, (_, i) => (
      <ScheduledImage key={i} src={`cold-before-return-${i}`} />
    ));
    const { container, rerender } = render(
      <>
        {covers}
        <StrictMode />
      </>,
    );
    await nextFrame();
    await nextFrame();
    await act(async () => {
      container.querySelectorAll("img[src]").forEach(fireEvent.load);
    });
    await nextFrame();
    expect(finish).toHaveLength(4);
    rerender(
      <>
        {covers}
        <StrictMode>
          <ScheduledImage src="return-under-cold" loading="lazy" />
        </StrictMode>
      </>,
    );
    const returning = container.querySelector("img:last-child")!;
    expect(returning.getAttribute("src")).toBe("return-under-cold");
    expect(container.querySelectorAll("img[src]")).toHaveLength(5);
    await act(async () => fireEvent.load(returning));
    expect(finish).toHaveLength(5);
    await act(async () => finish[4]());
    await nextFrame();
    expect(container.querySelectorAll("img[src]")).toHaveLength(5);
    await act(async () => finish.slice(0, 4).forEach((done) => done()));
  });

  it("switches to the page scroll root when its ref attaches", async () => {
    const root = document.createElement("div");
    document.body.append(root);
    const { container, rerender } = render(
      <PageScrollProvider element={null}>
        <ScheduledImage src="page-scroll-cover" loading="lazy" />
      </PageScrollProvider>,
      { container: root },
    );
    expect(observers[0].options.root).toBeNull();
    rerender(
      <PageScrollProvider element={root}>
        <ScheduledImage src="page-scroll-cover" loading="lazy" />
      </PageScrollProvider>,
    );
    expect(observers[0].targets.size).toBe(0);
    expect(observers[1].options).toEqual({ root, rootMargin: "400px" });
    expect(container.querySelector("img")?.hasAttribute("src")).toBe(false);
    await act(async () => observers[1].reveal());
    await nextFrame();
    expect(container.querySelector("img")?.getAttribute("src")).toBe(
      "page-scroll-cover",
    );
  });

  it("cancels queued sources and handles A→B→A before an earlier decode ends", async () => {
    let finishOld!: () => void;
    decode.mockImplementationOnce(
      () => new Promise((resolve) => (finishOld = resolve)),
    );
    const onLoad = vi.fn();
    const { container, rerender } = render(
      <ScheduledImage src="replace-a" onLoad={onLoad} />,
    );
    await nextFrame();
    const image = container.querySelector("img")!;
    await act(async () => fireEvent.load(image));
    rerender(<ScheduledImage src="replace-b" onLoad={onLoad} />);
    rerender(<ScheduledImage src="replace-a" onLoad={onLoad} />);
    await nextFrame();
    expect(image.getAttribute("src")).toBe("replace-a");
    await act(async () => finishOld());
    expect(onLoad).toHaveBeenCalledTimes(1);
    await act(async () => fireEvent.load(image));
    expect(onLoad).toHaveBeenCalledTimes(2);
  });

  it("releases failures and decode rejections so following covers still load", async () => {
    decode.mockRejectedValue(new Error("Decoder rejected"));
    const onError = vi.fn();
    const onLoad = vi.fn();
    const { container } = render(
      <>
        {Array.from({ length: 6 }, (_, i) => (
          <ScheduledImage
            key={i}
            src={`failure-${i}`}
            onError={onError}
            onLoad={onLoad}
          />
        ))}
      </>,
    );
    await nextFrame();
    await nextFrame();
    const images = container.querySelectorAll("img");
    await act(async () => {
      fireEvent.error(images[0]);
      fireEvent.error(images[0]);
      fireEvent.load(images[0]);
      fireEvent.load(images[1]);
    });
    await nextFrame();
    expect(container.querySelectorAll("img[src]")).toHaveLength(6);
    expect(onError).toHaveBeenCalledTimes(1);
    expect(onLoad).toHaveBeenCalledTimes(1);
  });

  it("retains displayed artwork while a replacement waits and remounts it before paint", async () => {
    const { container, rerender, unmount } = render(
      <ScheduledImage src="ready-old" />,
    );
    await nextFrame();
    await act(async () => fireEvent.load(container.querySelector("img")!));
    await nextFrame();
    rerender(<ScheduledImage src="ready-next" />);
    expect(container.querySelector("img")?.getAttribute("src")).toBe(
      "ready-old",
    );
    unmount();
    const cached = render(<ScheduledImage src="ready-old" loading="lazy" />);
    expect(cached.container.querySelector("img")?.getAttribute("src")).toBe(
      "ready-old",
    );
  });

  it("keeps a ready lazy source through StrictMode replay and observer delivery during decode", async () => {
    rememberReadyImage("strict-ready");
    let finish!: () => void;
    decode.mockImplementation(
      () => new Promise((resolve) => (finish = resolve)),
    );
    const onLoad = vi.fn();
    const { container } = render(
      <StrictMode>
        <ScheduledImage src="strict-ready" loading="lazy" onLoad={onLoad} />
      </StrictMode>,
    );
    const image = container.querySelector("img")!;
    expect(image.getAttribute("src")).toBe("strict-ready");
    expect(observers).toHaveLength(0);
    await act(async () => fireEvent.load(image));
    await act(async () => observers.forEach((observer) => observer.reveal()));
    expect(image.getAttribute("src")).toBe("strict-ready");
    await nextFrame();
    await act(async () => fireEvent.load(image));
    expect(decode).toHaveBeenCalledTimes(1);
    expect(onLoad).toHaveBeenCalledTimes(1);
    await act(async () => finish());
  });

  it("releases a cancelled decode once and clears an explicitly removed source", async () => {
    let finishOld!: () => void;
    decode.mockImplementationOnce(
      () => new Promise((resolve) => (finishOld = resolve)),
    );
    const { container, rerender } = render(
      <ScheduledImage src="removed-old" />,
    );
    await nextFrame();
    await act(async () => fireEvent.load(container.querySelector("img")!));
    rerender(
      <>
        <ScheduledImage />
        {Array.from({ length: 5 }, (_, i) => (
          <ScheduledImage key={i} src={`after-removed-${i}`} />
        ))}
      </>,
    );
    expect(container.querySelector("img")?.hasAttribute("src")).toBe(false);
    await nextFrame();
    await nextFrame();
    expect(container.querySelectorAll("img[src]")).toHaveLength(4);
    await act(async () => finishOld());
    await nextFrame();
    expect(container.querySelectorAll("img[src]")).toHaveLength(4);
  });

  it("drops departed queued covers before admitting the replacement page", async () => {
    const { rerender, container } = render(
      <>
        {Array.from({ length: 100 }, (_, i) => (
          <ScheduledImage key={i} src={`departed-${i}`} />
        ))}
      </>,
    );
    await nextFrame();
    rerender(<ScheduledImage key="replacement" src="replacement-page" />);
    await nextFrame();
    expect(container.querySelectorAll("img[src]")).toHaveLength(1);
    expect(container.querySelector("img")?.getAttribute("src")).toBe(
      "replacement-page",
    );
  });
});
