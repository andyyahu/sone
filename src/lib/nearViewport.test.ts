import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { observeNearViewport } from "./nearViewport";

const observers: ViewportObserver[] = [];
const cleanups: Array<() => void> = [];

class ViewportObserver {
  targets = new Set<Element>();
  disconnect = vi.fn(() => this.targets.clear());
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
  reveal(target: Element, isIntersecting = true) {
    this.callback(
      [{ target, isIntersecting } as IntersectionObserverEntry],
      this as unknown as IntersectionObserver,
    );
  }
}

function observe(element: Element, root: Element | null = null) {
  const ready = vi.fn();
  const cancel = observeNearViewport(element, ready, root);
  cleanups.push(cancel);
  return { ready, cancel };
}

function child(root: Element) {
  const element = document.createElement("img");
  root.append(element);
  return element;
}

beforeEach(() => {
  observers.length = 0;
  vi.stubGlobal("IntersectionObserver", ViewportObserver);
});

afterEach(() => {
  cleanups.splice(0).forEach((cancel) => cancel());
  vi.unstubAllGlobals();
});

describe("shared near-viewport observation", () => {
  it("prefetches against each scroll root while sharing observers within it", () => {
    const firstRoot = document.createElement("div");
    const secondRoot = document.createElement("div");
    const first = child(firstRoot);
    const second = child(firstRoot);
    const third = child(secondRoot);
    const firstSubscription = observe(first, firstRoot);
    observe(second, firstRoot);
    observe(third, secondRoot);

    expect(observers).toHaveLength(2);
    expect(observers[0].options).toEqual({
      root: firstRoot,
      rootMargin: "400px",
    });
    expect(observers[1].options.root).toBe(secondRoot);
    expect(observers[0].targets).toEqual(new Set([first, second]));
    observers[0].reveal(first, false);
    expect(firstSubscription.ready).not.toHaveBeenCalled();
    observers[0].reveal(first);
    observers[0].reveal(first);
    expect(firstSubscription.ready).toHaveBeenCalledTimes(1);
    expect(observers[0].targets).toEqual(new Set([second]));
  });

  it("disconnects an idle root without cancelling another root", () => {
    const firstRoot = document.createElement("div");
    const secondRoot = document.createElement("div");
    const first = child(firstRoot);
    const second = child(secondRoot);
    const firstSubscription = observe(first, firstRoot);
    const secondSubscription = observe(second, secondRoot);
    firstSubscription.cancel();
    expect(observers[0].disconnect).toHaveBeenCalledTimes(1);
    expect(observers[1].disconnect).not.toHaveBeenCalled();
    observe(first, firstRoot);
    expect(observers).toHaveLength(3);
    firstSubscription.cancel();
    expect(observers[2].targets.has(first)).toBe(true);
    observers[1].reveal(second);
    expect(secondSubscription.ready).toHaveBeenCalledTimes(1);
    expect(observers[1].disconnect).toHaveBeenCalledTimes(1);
  });

  it("uses the window viewport for portals outside the provided scroll root", () => {
    const root = document.createElement("div");
    const portal = document.createElement("img");
    const ordinary = document.createElement("img");
    const subscription = observe(portal, root);
    observe(ordinary);
    expect(observers).toHaveLength(1);
    expect(observers[0].options.root).toBeNull();
    observers[0].reveal(portal);
    expect(subscription.ready).toHaveBeenCalledTimes(1);
  });

  it("does not let stale cleanup cancel a replacement subscription", () => {
    const element = document.createElement("img");
    const oldSubscription = observe(element);
    const newSubscription = observe(element);
    oldSubscription.cancel();
    expect(observers[0].targets.has(element)).toBe(true);
    observers[0].reveal(element);
    expect(oldSubscription.ready).not.toHaveBeenCalled();
    expect(newSubscription.ready).toHaveBeenCalledTimes(1);
  });
});
