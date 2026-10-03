import {
  useRef,
  useState,
  useEffect,
  type RefObject,
  type CSSProperties,
} from "react";
import { useEscapeDismiss } from "./useEscapeDismiss";
import { DISMISS_PRIORITY } from "../lib/dismissStack";

interface UseContextMenuOptions {
  cursorPosition?: { x: number; y: number };
  anchorRef?: RefObject<HTMLElement | null>;
  anchorGap?: number;
  ignoreRefs?: RefObject<HTMLElement | null>[];
  suppressClose?: boolean;
  onClose: () => void;
}

export function useContextMenu({
  cursorPosition,
  anchorRef,
  anchorGap = 4,
  ignoreRefs,
  suppressClose,
  onClose,
}: UseContextMenuOptions) {
  const menuRef = useRef<HTMLDivElement>(null);
  const [isPositioned, setIsPositioned] = useState(false);
  const [pos, setPos] = useState<{
    top: number;
    left: number;
    origin: "top left" | "bottom left";
  }>({ top: -9999, left: -9999, origin: "top left" });

  // Position the menu after first paint so we can measure its size.
  useEffect(() => {
    const raf = requestAnimationFrame(() => {
      const menu = menuRef.current;
      if (!menu) return;

      const zoom = parseFloat(document.documentElement.style.zoom || "1");
      const menuRect = menu.getBoundingClientRect();
      // clientX/Y, getBoundingClientRect, and innerWidth/Height return VISUAL
      // pixels (× zoom); position:fixed top/left are LAYOUT pixels. Divide
      // every visual-space read by zoom so all the math below is layout-px.
      const menuWidth = menuRect.width / zoom || 240;
      const menuHeight = menuRect.height / zoom || 300;
      const viewW = window.innerWidth / zoom;
      const viewH = window.innerHeight / zoom;
      const pad = 8;

      let top: number;
      let left: number;

      if (cursorPosition) {
        top = cursorPosition.y / zoom;
        left = cursorPosition.x / zoom;
      } else if (anchorRef?.current) {
        const rect = anchorRef.current.getBoundingClientRect();
        top = rect.bottom / zoom + anchorGap;
        left = rect.right / zoom - menuWidth;
      } else {
        return;
      }

      // Clamp horizontally
      if (left < pad) left = pad;
      if (left + menuWidth > viewW - pad) left = viewW - menuWidth - pad;

      // Clamp vertically — flip upward if overflowing bottom
      if (top + menuHeight > viewH - pad) {
        if (cursorPosition) {
          top = cursorPosition.y / zoom - menuHeight;
        } else if (anchorRef?.current) {
          const rect = anchorRef.current.getBoundingClientRect();
          top = rect.top / zoom - menuHeight - anchorGap;
        }
      }
      if (top < pad) top = pad;

      let origin: "top left" | "bottom left" = "top left";
      if (cursorPosition) {
        if (top < cursorPosition.y / zoom) origin = "bottom left";
      } else if (anchorRef?.current) {
        const anchorTop = anchorRef.current.getBoundingClientRect().top / zoom;
        if (top < anchorTop) origin = "bottom left";
      }

      setPos({ top, left, origin });
      setIsPositioned(true);
    });

    return () => cancelAnimationFrame(raf);
  }, [cursorPosition, anchorRef, anchorGap]);

  useEscapeDismiss(!suppressClose, onClose, DISMISS_PRIORITY.contextMenu);

  // Dismiss on click-outside
  useEffect(() => {
    if (suppressClose) return;

    const handleMouseDown = (e: MouseEvent) => {
      const target = e.target as Node;
      if (menuRef.current?.contains(target)) return;
      if (anchorRef?.current?.contains(target)) return;
      if (ignoreRefs?.some((r) => r.current?.contains(target))) return;
      onClose();
    };

    document.addEventListener("mousedown", handleMouseDown);
    return () => {
      document.removeEventListener("mousedown", handleMouseDown);
    };
  }, [onClose, anchorRef, ignoreRefs, suppressClose]);

  const reduceMotion =
    window.matchMedia?.("(prefers-reduced-motion: reduce)")?.matches ?? false;

  const style: CSSProperties = {
    position: "fixed",
    top: pos.top,
    left: pos.left,
    opacity: isPositioned ? 1 : 0,
    transformOrigin: pos.origin,
    animation:
      isPositioned && !reduceMotion
        ? "menuIn 160ms var(--ease-settle) both"
        : undefined,
  };

  return { menuRef, style, isPositioned };
}
