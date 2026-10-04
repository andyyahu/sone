import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import VirtualSidebarItems from "./VirtualSidebarItems";

const keyReads = vi.hoisted(() => vi.fn());
vi.mock("@tanstack/react-virtual", async (importOriginal) => {
  const original =
    await importOriginal<typeof import("@tanstack/react-virtual")>();
  type Options = Parameters<typeof original.useVirtualizer>[0];
  type GetKey = NonNullable<Options["getItemKey"]>;
  const observedKeys = new WeakMap<GetKey, GetKey>();
  return {
    ...original,
    useVirtualizer: (options: Options) => {
      const getKey = options.getItemKey!;
      let observed = observedKeys.get(getKey);
      if (!observed) {
        observed = (index) => {
          keyReads(index);
          return getKey(index);
        };
        observedKeys.set(getKey, observed);
      }
      // Keep callback identity intact while exercising the real virtualizer.
      return original.useVirtualizer({ ...options, getItemKey: observed });
    },
  };
});

class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}

function scroller() {
  const element = document.createElement("div");
  document.body.appendChild(element);
  for (const name of ["clientHeight", "offsetHeight"]) {
    Object.defineProperty(element, name, { get: () => 456 });
  }
  element.getBoundingClientRect = () =>
    ({ top: 0, left: 0, width: 280, height: 456 }) as DOMRect;
  element.scrollTo = vi.fn();
  return element;
}

const buttons = (count: number) =>
  Array.from({ length: count }, (_, index) => (
    <button key={index}>Album {index}</button>
  ));

beforeEach(() => {
  vi.stubGlobal("ResizeObserver", ResizeObserverStub);
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
});
afterEach(() => {
  // Finish virtual-core's debounced scroll notification before jsdom teardown.
  act(() => vi.runOnlyPendingTimers());
  cleanup();
  vi.clearAllTimers();
  vi.useRealTimers();
  document.body.replaceChildren();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("sidebar collection windowing", () => {
  it("keeps mounted rows bounded for large collections and preserves total height", () => {
    const element = scroller();
    const { container } = render(
      <VirtualSidebarItems scrollElement={element}>
        {buttons(1000)}
      </VirtualSidebarItems>,
    );
    const rows = container.querySelectorAll("[data-sidebar-index]");
    expect(rows.length).toBeGreaterThan(0);
    expect(rows.length).toBeLessThan(30);
    expect(
      container.querySelector<HTMLElement>("[data-virtual-sidebar]")?.style
        .height,
    ).toBe("56999px");
  });

  it("keeps a focused row and its tab neighbors mounted after scrolling away", () => {
    const element = scroller();
    const { container } = render(
      <VirtualSidebarItems scrollElement={element}>
        {buttons(1000)}
      </VirtualSidebarItems>,
      { container: element },
    );
    const focused = container.querySelector<HTMLButtonElement>(
      "[data-sidebar-index='2'] button",
    )!;
    act(() => focused.focus());
    act(() => {
      element.scrollTop = 5700;
      fireEvent.scroll(element);
    });
    expect(document.activeElement).toBe(focused);
    expect(container.querySelector("[data-sidebar-index='1']")).not.toBeNull();
    expect(container.querySelector("[data-sidebar-index='3']")).not.toBeNull();
    expect(
      container.querySelector("[data-sidebar-index='100']"),
    ).not.toBeNull();
    expect(
      container.querySelectorAll("[data-sidebar-index]").length,
    ).toBeLessThan(40);
  });

  it("reuses measurements while scrolling and refreshes keys when items reorder", () => {
    const element = scroller();
    const items = buttons(1000);
    const { container, rerender } = render(
      <VirtualSidebarItems scrollElement={element}>
        {items}
      </VirtualSidebarItems>,
    );
    keyReads.mockClear();
    for (const position of [1140, 3420, 5700]) {
      act(() => {
        element.scrollTop = position;
        fireEvent.scroll(element);
      });
    }
    expect(keyReads.mock.calls.length).toBe(0);
    const originalRow = container.querySelector("[data-sidebar-index='100']");
    expect(originalRow?.textContent).toBe("Album 100");

    const reordered = [...items];
    [reordered[100], reordered[101]] = [reordered[101], reordered[100]];
    rerender(
      <VirtualSidebarItems scrollElement={element}>
        {reordered}
      </VirtualSidebarItems>,
    );
    expect(keyReads).toHaveBeenCalled();
    expect(container.querySelector("[data-sidebar-index='101']")).toBe(
      originalRow,
    );
    expect(
      container.querySelector("[data-sidebar-index='100']")?.textContent,
    ).toBe("Album 101");
  });

  it("does not reset a scrolled collection when virtualization attaches or more items arrive", () => {
    const element = scroller();
    element.scrollTop = 5700;
    const { container, rerender } = render(
      <VirtualSidebarItems scrollElement={element}>
        {buttons(200)}
      </VirtualSidebarItems>,
    );
    expect(
      container.querySelector("[data-sidebar-index='100']"),
    ).not.toBeNull();
    rerender(
      <VirtualSidebarItems scrollElement={element}>
        {buttons(500)}
      </VirtualSidebarItems>,
    );
    expect(element.scrollTop).toBe(5700);
    expect(
      container.querySelector("[data-sidebar-index='100']"),
    ).not.toBeNull();
    expect(
      container.querySelector<HTMLElement>("[data-virtual-sidebar]")?.style
        .height,
    ).toBe("28499px");
  });

  it("renders small collections directly", () => {
    const { container } = render(
      <VirtualSidebarItems scrollElement={scroller()}>
        {buttons(10)}
      </VirtualSidebarItems>,
    );
    expect(container.querySelectorAll("button")).toHaveLength(10);
    expect(container.querySelector("[data-virtual-sidebar]")).toBeNull();
  });
});
