import {
  Children,
  isValidElement,
  useCallback,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { defaultRangeExtractor, useVirtualizer } from "@tanstack/react-virtual";

interface Props {
  children: ReactNode;
  scrollElement: HTMLElement | null;
}

export default function VirtualSidebarItems({
  children,
  scrollElement,
}: Props) {
  const items = useMemo(() => Children.toArray(children), [children]);
  if (!scrollElement || items.length < 40) return <>{children}</>;
  return <WindowedItems items={items} scrollElement={scrollElement} />;
}

function WindowedItems({
  items,
  scrollElement,
}: {
  items: ReactNode[];
  scrollElement: HTMLElement;
}) {
  const listRef = useRef<HTMLDivElement>(null);
  const [scrollMargin, setScrollMargin] = useState(0);
  const [focusedIndex, setFocusedIndex] = useState<number | null>(null);

  useLayoutEffect(() => {
    const list = listRef.current;
    if (!list) return;
    const measure = () => {
      // Layout coordinates stay consistent with scrollTop under CSS zoom.
      let top = 0;
      let node: HTMLElement | null = list;
      while (node && node !== scrollElement) {
        top += node.offsetTop;
        node = node.offsetParent as HTMLElement | null;
      }
      setScrollMargin(top);
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(scrollElement);
    if (list.parentElement) observer.observe(list.parentElement);
    return () => observer.disconnect();
  }, [scrollElement]);

  // Stable between scroll updates, so virtual-core retains all measurements.
  const getItemKey = useCallback(
    (index: number) => {
      const item = items[index];
      return isValidElement(item) && item.key != null ? item.key : index;
    },
    [items],
  );

  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => scrollElement,
    // Every library button contains a 40px cover plus 8px vertical padding.
    estimateSize: () => 56,
    gap: 1,
    overscan: 6,
    scrollMargin,
    initialOffset: () => scrollElement.scrollTop,
    getItemKey,
    rangeExtractor: (range) => {
      const indexes = defaultRangeExtractor(range);
      if (focusedIndex === null) return indexes;
      // Keep focus mounted when scrolling, plus the adjacent buttons so Tab
      // and Shift+Tab can move beyond the current rendered window.
      for (let i = focusedIndex - 1; i <= focusedIndex + 1; i++) {
        if (i >= 0 && i < items.length) indexes.push(i);
      }
      return [...new Set(indexes)].sort((a, b) => a - b);
    },
  });

  return (
    <div
      ref={listRef}
      data-virtual-sidebar=""
      className="relative"
      style={{ height: virtualizer.getTotalSize() }}
      onFocusCapture={(event) => {
        const row = (event.target as HTMLElement).closest<HTMLElement>(
          "[data-sidebar-index]",
        );
        if (row) setFocusedIndex(Number(row.dataset.sidebarIndex));
      }}
      onBlurCapture={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget as Node | null)) {
          setFocusedIndex(null);
        }
      }}
    >
      {virtualizer.getVirtualItems().map((row) => (
        <div
          key={row.key}
          data-sidebar-index={row.index}
          className="absolute top-0 left-0 w-full"
          style={{
            height: row.size,
            transform: `translateY(${row.start - scrollMargin}px)`,
          }}
        >
          {items[row.index]}
        </div>
      ))}
    </div>
  );
}
