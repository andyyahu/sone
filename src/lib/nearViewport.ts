// Share one observer per scroll root. A viewport observer's rootMargin cannot
// extend an inner scroller's clip, so page covers need that scroller as root to
// start loading before they become visible.
interface ObservationGroup {
  pending: Map<Element, () => void>;
  observer: IntersectionObserver;
}
const groups = new Map<Element | null, ObservationGroup>();

export function observeNearViewport(
  element: Element,
  ready: () => void,
  scrollRoot: Element | null = null,
) {
  if (typeof IntersectionObserver === "undefined") {
    ready();
    return () => {};
  }

  // Portals can inherit page context without being descendants of its scroller.
  const root = scrollRoot?.contains(element) ? scrollRoot : null;
  let group = groups.get(root);
  if (!group) {
    const pending = new Map<Element, () => void>();
    const observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (!entry.isIntersecting) continue;
          const callback = pending.get(entry.target);
          if (!callback) continue;
          pending.delete(entry.target);
          observer.unobserve(entry.target);
          callback();
        }
        disconnectIfIdle();
      },
      { root, rootMargin: "400px" },
    );
    group = { pending, observer };
    groups.set(root, group);
  }

  const observedGroup = group;
  function disconnectIfIdle() {
    if (observedGroup.pending.size !== 0) return;
    observedGroup.observer.disconnect();
    if (groups.get(root) === observedGroup) groups.delete(root);
  }

  observedGroup.pending.set(element, ready);
  observedGroup.observer.observe(element);

  return () => {
    // A stale cleanup must not remove a newer subscription for this element.
    if (observedGroup.pending.get(element) !== ready) return;
    observedGroup.pending.delete(element);
    observedGroup.observer.unobserve(element);
    disconnectIfIdle();
  };
}
