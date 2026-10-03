import { act, cleanup, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { PageScrollProvider } from "../contexts/PageScrollContext";
import MediaGrid from "./MediaGrid";
import { mediaGridColumns } from "./mediaGridLayout";

const observers = new Set<TestResizeObserver>();

class TestResizeObserver {
  private targets = new Set<Element>();

  constructor(private callback: ResizeObserverCallback) {
    observers.add(this);
  }

  observe(target: Element) {
    this.targets.add(target);
  }

  unobserve(target: Element) {
    this.targets.delete(target);
  }

  disconnect() {
    this.targets.clear();
    observers.delete(this);
  }

  fire() {
    const entries = [...this.targets].map((target) => ({
      target,
      borderBoxSize: [
        {
          inlineSize: (target as HTMLElement).clientWidth,
          blockSize: (target as HTMLElement).offsetHeight,
        },
      ],
    }));
    if (entries.length) {
      this.callback(
        entries as unknown as ResizeObserverEntry[],
        this as unknown as ResizeObserver,
      );
    }
  }
}

describe("media grid column breakpoints", () => {
  it("matches the existing container queries on both sides of each boundary", () => {
    expect(mediaGridColumns(0)).toBe(1);
    [384, 512, 768, 1024, 1280].forEach((width, index) => {
      expect(mediaGridColumns(width - 1)).toBe(index + 1);
      expect(mediaGridColumns(width)).toBe(index + 2);
    });
    expect(mediaGridColumns(4000)).toBe(6);
  });

  it("uses rem units like the ordinary CSS grid", () => {
    expect(mediaGridColumns(768, 20)).toBe(3);
    expect(mediaGridColumns(960, 20)).toBe(4);
  });
});

describe("MediaGrid virtualization", () => {
  let scroller: HTMLDivElement;
  let width: number;
  let rowHeight: number;
  let headerHeight: number;

  beforeEach(() => {
    width = 900;
    rowHeight = 270;
    headerHeight = 320;
    scroller = document.createElement("div");
    document.body.append(scroller);
    scroller.scrollTo = vi.fn();
    vi.stubGlobal("ResizeObserver", TestResizeObserver);
    vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(
      () => width,
    );
    vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockImplementation(
      function (this: HTMLElement) {
        return this === scroller
          ? 600
          : this.hasAttribute("data-index")
            ? rowHeight
            : 0;
      },
    );
    vi.spyOn(HTMLElement.prototype, "offsetTop", "get").mockImplementation(
      function (this: HTMLElement) {
        return this.hasAttribute("data-virtual-media-grid") ? headerHeight : 0;
      },
    );
    vi.spyOn(HTMLElement.prototype, "offsetParent", "get").mockImplementation(
      function (this: HTMLElement) {
        return this === scroller ? null : scroller;
      },
    );
    // A zoomed rect disagrees with layout pixels; using it would open 6 columns
    // and double row measurements instead of keeping the expected 4 columns.
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(
      function (this: HTMLElement) {
        return {
          width: width * 2,
          height: this.offsetHeight * 2,
          top: (headerHeight - scroller.scrollTop) * 2,
          left: 0,
          right: width * 2,
          bottom: this.offsetHeight * 2,
          x: 0,
          y: 0,
          toJSON: () => ({}),
        };
      },
    );
  });

  afterEach(() => {
    cleanup();
    scroller.remove();
    observers.clear();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  function cards(count: number) {
    return Array.from({ length: count }, (_, i) => (
      <button key={i} data-card={i}>
        Album {i}
      </button>
    ));
  }

  function page(count: number) {
    return (
      <PageScrollProvider element={scroller}>
        <MediaGrid>{cards(count)}</MediaGrid>
        <div data-testid="pagination-sentinel" />
      </PageScrollProvider>
    );
  }

  function notifyResize() {
    act(() => [...observers].forEach((observer) => observer.fire()));
  }

  it("mounts a bounded window of cards and keeps layout pixels under zoom", () => {
    const { container } = render(page(1000));
    const mounted = container.querySelectorAll("[data-card]");
    expect(mounted.length).toBeGreaterThan(0);
    expect(mounted.length).toBeLessThan(40);
    const row = container.querySelector<HTMLElement>("[data-index]")!;
    expect(row.children).toHaveLength(4);
    expect(row.style.gridTemplateColumns).toBe("repeat(4, minmax(0, 1fr))");
    const second = container.querySelector<HTMLElement>('[data-index="1"]')!;
    expect(second.style.transform).toBe("translateY(290px)");
  });

  it("regroups rows and discards stale heights when the available width changes", () => {
    const { container } = render(page(1000));
    width = 700;
    rowHeight = 340;
    notifyResize();
    const first = container.querySelector<HTMLElement>('[data-index="0"]')!;
    const second = container.querySelector<HTMLElement>('[data-index="1"]')!;
    expect(first.children).toHaveLength(3);
    expect(second.firstElementChild?.getAttribute("data-card")).toBe("3");
    expect(second.style.transform).toBe("translateY(360px)");
    expect(container.querySelectorAll("[data-card]").length).toBeLessThan(30);
  });

  it("retains a restored offset when attaching and windows deeply scrolled content", () => {
    scroller.scrollTop = 12000;
    const { container } = render(page(1000));
    expect(scroller.scrollTo).toHaveBeenCalled();
    expect(scroller.scrollTo).not.toHaveBeenCalledWith(
      expect.objectContaining({ top: 0 }),
    );
    const first = Number(
      container.querySelector("[data-card]")!.getAttribute("data-card"),
    );
    expect(first).toBeGreaterThan(100);

    act(() => {
      scroller.scrollTop = 18000;
      scroller.dispatchEvent(new Event("scroll"));
    });
    notifyResize();
    const after = Number(
      container.querySelector("[data-card]")!.getAttribute("data-card"),
    );
    expect(after).toBeGreaterThan(first);
    expect(container.querySelectorAll("[data-card]").length).toBeLessThan(40);
  });

  it("extends the spacer before an external pagination sentinel as pages append", () => {
    const { container, rerender, getByTestId } = render(page(60));
    const grid = container.querySelector<HTMLElement>(
      "[data-virtual-media-grid]",
    )!;
    const before = parseFloat(grid.style.height);
    rerender(page(120));
    expect(parseFloat(grid.style.height)).toBeGreaterThan(before * 1.8);
    expect(grid.nextElementSibling).toBe(getByTestId("pagination-sentinel"));
    expect(container.querySelectorAll("[data-card]").length).toBeLessThan(40);
  });

  it("keeps cached row sizes when the page is temporarily hidden", () => {
    const { container } = render(page(1000));
    const grid = container.querySelector<HTMLElement>(
      "[data-virtual-media-grid]",
    )!;
    const before = grid.style.height;
    width = 0;
    rowHeight = 0;
    notifyResize();
    expect(grid.style.height).toBe(before);
  });

  it("retains keyboard focus and an adjacent row when scrolling it offscreen", () => {
    const { container } = render(page(1000));
    const first =
      container.querySelector<HTMLButtonElement>('[data-card="0"]')!;
    act(() => first.focus());
    act(() => {
      scroller.scrollTop = 12000;
      scroller.dispatchEvent(new Event("scroll"));
    });
    expect(document.activeElement).toBe(first);
    expect(container.querySelector('[data-card="4"]')).not.toBeNull();
    expect(container.querySelectorAll("[data-card]").length).toBeLessThan(52);

    // Advancing into the retained next row must make its own successor
    // available for the next Tab press, even away from the current viewport.
    const next = container.querySelector<HTMLButtonElement>('[data-card="4"]')!;
    act(() => next.focus());
    expect(container.querySelector('[data-card="8"]')).not.toBeNull();
    act(() => next.blur());
    expect(container.querySelector('[data-card="0"]')).toBeNull();
  });

  it("preserves ordinary CSS grid layout for small shelves and standalone use", () => {
    const { container, rerender } = render(page(12));
    expect(container.querySelectorAll("[data-card]")).toHaveLength(12);
    expect(container.querySelector("[data-virtual-media-grid]")).toBeNull();
    rerender(<MediaGrid>{cards(50)}</MediaGrid>);
    expect(container.querySelectorAll("[data-card]")).toHaveLength(50);
    expect(container.querySelector("[data-virtual-media-grid]")).toBeNull();
  });
});
