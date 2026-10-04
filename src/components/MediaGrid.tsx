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
import { usePageScrollElement } from "../contexts/PageScrollContext";
import { mediaGridColumns } from "./mediaGridLayout";

const GRID_CLASSES =
  "grid grid-cols-1 @sm:grid-cols-2 @lg:grid-cols-3 @3xl:grid-cols-4 @5xl:grid-cols-5 @7xl:grid-cols-6 gap-5";
const VIRTUALIZE_AT = 40;
const GAP = 20;

interface MediaGridProps {
  children: ReactNode;
}

/** Small shelves keep ordinary grid layout; library pages window whole rows. */
export default function MediaGrid({ children }: MediaGridProps) {
  const scrollElement = usePageScrollElement();
  const items = useMemo(() => Children.toArray(children), [children]);

  if (scrollElement && items.length >= VIRTUALIZE_AT) {
    return <VirtualMediaGrid items={items} scrollElement={scrollElement} />;
  }

  return (
    <div className="@container">
      <div className={GRID_CLASSES}>{children}</div>
    </div>
  );
}

function VirtualMediaGrid({
  items,
  scrollElement,
}: {
  items: ReactNode[];
  scrollElement: HTMLElement;
}) {
  const gridRef = useRef<HTMLDivElement>(null);
  const [focusedRow, setFocusedRow] = useState<number | null>(null);
  const [layout, setLayout] = useState({
    width: 0,
    columns: 1,
    scrollMargin: 0,
  });

  useLayoutEffect(() => {
    const grid = gridRef.current;
    if (!grid) return;
    const measure = () => {
      // Layout pixels throughout: rects include :root's CSS zoom and scrolling.
      const width = grid.clientWidth;
      if (!width) return;
      let node: HTMLElement | null = grid;
      let scrollMargin = 0;
      while (node && node !== scrollElement) {
        scrollMargin += node.offsetTop;
        node = node.offsetParent as HTMLElement | null;
      }
      const rem =
        parseFloat(getComputedStyle(document.documentElement).fontSize) || 16;
      const columns = mediaGridColumns(width, rem);
      setLayout((previous) =>
        previous.width === width &&
        previous.columns === columns &&
        previous.scrollMargin === scrollMargin
          ? previous
          : { width, columns, scrollMargin },
      );
    };
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(grid);
    observer.observe(scrollElement);
    if (scrollElement.firstElementChild) {
      observer.observe(scrollElement.firstElementChild);
    }
    return () => observer.disconnect();
  }, [scrollElement]);

  const { columns, width, scrollMargin } = layout;
  const getItemKey = useCallback(
    (index: number) => {
      const first = items[index * columns];
      return `${columns}:${isValidElement(first) ? first.key : index}`;
    },
    [items, columns],
  );
  const virtualizer = useVirtualizer<HTMLElement, HTMLDivElement>({
    count: Math.ceil(items.length / columns),
    getScrollElement: () => scrollElement,
    initialOffset: () => scrollElement.scrollTop,
    scrollMargin,
    gap: GAP,
    overscan: 2,
    rangeExtractor: (range) => {
      const visible = defaultRangeExtractor(range);
      if (focusedRow === null) return visible;
      // Retain the focused row and its neighbours so scrolling cannot remove
      // focus, and Tab can advance into the next virtual row in DOM order.
      return [
        ...new Set([...visible, focusedRow - 1, focusedRow, focusedRow + 1]),
      ]
        .filter((index) => index >= 0 && index < range.count)
        .sort((a, b) => a - b);
    },
    getItemKey,
    estimateSize: (index) => {
      const cardWidth = width ? (width - GAP * (columns - 1)) / columns : 240;
      // Image aspect is known before mounting. Actual row measurements account
      // for titles, subtitles and custom cards without forcing fixed heights.
      const row = items.slice(index * columns, (index + 1) * columns);
      const imageRatio = Math.max(
        ...row.map((item) => {
          if (!isValidElement<{ aspect?: string }>(item)) return 1;
          return item.props.aspect === "video"
            ? 9 / 16
            : item.props.aspect === "promo"
              ? 8 / 11
              : 1;
        }),
      );
      return Math.max(80, (cardWidth - 24) * imageRatio + 92);
    },
    measureElement: (element, entry, instance) => {
      const height =
        entry?.borderBoxSize?.[0]?.blockSize || element.offsetHeight;
      const index = Number(element.dataset.index);
      // Fullscreen video hides the page with display:none. Keep its row sizes
      // while hidden so showing the library again preserves its scroll range.
      return (
        height ||
        instance.measurementsCache[index]?.size ||
        instance.options.estimateSize(index)
      );
    },
  });

  // Width changes reflow both the column grouping and wrapped subtitles. Old
  // measurements must not survive a sidebar resize or a UI zoom adjustment.
  useLayoutEffect(() => {
    virtualizer.measure();
    // React has already attached the row refs before this layout effect. A
    // cache reset must immediately remeasure them: an unchanged row height
    // will not necessarily cause another ResizeObserver notification.
    for (const row of gridRef.current?.children ?? []) {
      virtualizer.measureElement(row as HTMLDivElement);
    }
  }, [width, columns, virtualizer]);

  return (
    <div
      ref={gridRef}
      className="relative"
      data-virtual-media-grid
      style={{ height: virtualizer.getTotalSize() }}
      onFocusCapture={(event) => {
        const row = (event.target as HTMLElement).closest<HTMLElement>(
          "[data-index]",
        );
        if (row) setFocusedRow(Number(row.dataset.index));
      }}
      onBlurCapture={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget as Node | null)) {
          setFocusedRow(null);
        }
      }}
    >
      {virtualizer.getVirtualItems().map((row) => (
        <div
          key={row.key}
          ref={virtualizer.measureElement}
          data-index={row.index}
          className="absolute top-0 left-0 w-full grid gap-5"
          style={{
            gridTemplateColumns: `repeat(${columns}, minmax(0, 1fr))`,
            transform: `translateY(${row.start - scrollMargin}px)`,
          }}
        >
          {items.slice(row.index * columns, (row.index + 1) * columns)}
        </div>
      ))}
    </div>
  );
}

/** Reserve the same image and text height as a one-line MediaCard. */
export function MediaCardSkeleton() {
  return (
    <div className="p-3">
      <div className="aspect-square bg-th-surface-hover rounded-md mb-3" />
      <div className="h-[21px] flex items-center mb-1">
        <div className="h-4 w-3/4 bg-th-surface-hover rounded" />
      </div>
      <div className="h-[18px] flex items-center">
        <div className="h-3 w-1/2 bg-th-surface-hover rounded" />
      </div>
    </div>
  );
}

/** Loading skeleton for the media grid. */
export function MediaGridSkeleton({ count = 18 }: { count?: number }) {
  return (
    <MediaGrid>
      {Array.from({ length: count }).map((_, i) => (
        <MediaCardSkeleton key={i} />
      ))}
    </MediaGrid>
  );
}

/** Empty state for the media grid. */
export function MediaGridEmpty({
  message = "No items found",
}: {
  message?: string;
}) {
  return (
    <div className="text-center py-12">
      <p className="text-th-text-muted text-sm">{message}</p>
    </div>
  );
}

/** Error state for the media grid. */
export function MediaGridError({ error }: { error: string }) {
  return (
    <div className="text-center py-12">
      <p className="text-th-text-muted text-sm">Failed to load content</p>
      <p className="text-th-text-faint text-xs mt-1">{error}</p>
    </div>
  );
}
