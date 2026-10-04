import { StrictMode } from "react";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
} from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DISMISS_PRIORITY, registerDismissable } from "../../lib/dismissStack";
import SettingsSheet from "./SettingsSheet";

vi.mock("./PlaybackTab", () => ({
  default: () => (
    <>
      <button>Last setting</button>
      <button disabled>Disabled setting</button>
      <div style={{ display: "none" }}>
        <button>Hidden setting</button>
      </div>
    </>
  ),
}));
vi.mock("./ThemesTab", () => ({
  default: () => <button>Theme setting</button>,
}));
vi.mock("./ScrobbleTab", () => ({ default: () => null }));
vi.mock("./DiscordTab", () => ({ default: () => null }));
vi.mock("./GeneralTab", () => ({ default: () => null }));
vi.mock("./NetworkTab", () => ({ default: () => null }));
vi.mock("./UtilitiesTab", () => ({ default: () => null }));
vi.mock("./McpTab", () => ({ default: () => null }));
vi.mock("./OverlayTab", () => ({ default: () => null }));

let reduceMotion = false;
const motionListeners = new Set<() => void>();
let trigger: HTMLButtonElement;

beforeEach(() => {
  vi.useFakeTimers();
  reduceMotion = false;
  motionListeners.clear();
  vi.stubGlobal("matchMedia", () => ({
    get matches() {
      return reduceMotion;
    },
    addEventListener: (_event: string, fn: () => void) =>
      motionListeners.add(fn),
    removeEventListener: (_event: string, fn: () => void) =>
      motionListeners.delete(fn),
  }));
  trigger = document.createElement("button");
  trigger.textContent = "Open settings";
  document.body.append(trigger);
  trigger.focus();
});
afterEach(() => {
  cleanup();
  trigger.remove();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("Settings dialog lifecycle", () => {
  it("names the dialog, focuses close, contains Tab and restores the opener", () => {
    const { rerender } = render(<SettingsSheet open onClose={vi.fn()} />);
    const dialog = screen.getByRole("dialog", { name: "Settings" });
    expect(dialog.getAttribute("aria-modal")).toBe("true");
    const close = screen.getByRole("button", { name: "Close settings" });
    const last = screen.getByRole("button", { name: "Last setting" });
    expect(document.activeElement).toBe(close);
    fireEvent.keyDown(close, { key: "Tab", shiftKey: true });
    expect(document.activeElement).toBe(last);
    fireEvent.keyDown(last, { key: "Tab" });
    expect(document.activeElement).toBe(close);
    trigger.focus();
    expect(document.activeElement).toBe(close);
    rerender(<SettingsSheet open={false} onClose={vi.fn()} />);
    expect(document.activeElement).toBe(trigger);
  });

  it("keeps the inert exit for 120ms and cancels it when reopened", () => {
    const { rerender, container } = render(
      <SettingsSheet open onClose={vi.fn()} />,
    );
    rerender(<SettingsSheet open={false} onClose={vi.fn()} />);
    expect(
      container.querySelector("[data-state='closed'][inert]"),
    ).toBeTruthy();
    expect(screen.queryByRole("dialog")).toBeNull();
    act(() => vi.advanceTimersByTime(119));
    expect(container.querySelector("[role='dialog']")).toBeTruthy();
    rerender(<SettingsSheet open onClose={vi.fn()} />);
    act(() => vi.advanceTimersByTime(1));
    expect(screen.getByRole("dialog")).toBeTruthy();
    expect(document.activeElement).toBe(
      screen.getByRole("button", { name: "Close settings" }),
    );
    rerender(<SettingsSheet open={false} onClose={vi.fn()} />);
    act(() => vi.advanceTimersByTime(120));
    expect(container.querySelector("[role='dialog']")).toBeNull();
    expect(document.activeElement).toBe(trigger);
    rerender(<SettingsSheet open onClose={vi.fn()} />);
    expect(document.activeElement).toBe(
      screen.getByRole("button", { name: "Close settings" }),
    );
  });

  it("closes immediately for reduced motion, including preference changes during exit", () => {
    reduceMotion = true;
    const { rerender, container } = render(
      <SettingsSheet open onClose={vi.fn()} />,
    );
    rerender(<SettingsSheet open={false} onClose={vi.fn()} />);
    expect(container.querySelector("[role='dialog']")).toBeNull();
    expect(document.activeElement).toBe(trigger);
    act(() => {
      reduceMotion = false;
      motionListeners.forEach((fn) => fn());
    });
    rerender(<SettingsSheet open onClose={vi.fn()} />);
    rerender(<SettingsSheet open={false} onClose={vi.fn()} />);
    expect(container.querySelector("[role='dialog']")).toBeTruthy();
    act(() => {
      reduceMotion = true;
      motionListeners.forEach((fn) => fn());
    });
    expect(container.querySelector("[role='dialog']")).toBeNull();
  });

  it("leaves focus and Escape to a higher portal and ignores backdrop dismissal beneath it", () => {
    const onClose = vi.fn();
    const { container } = render(<SettingsSheet open onClose={onClose} />);
    const portal = document.createElement("button");
    portal.textContent = "Portal choice";
    document.body.append(portal);
    const closePortal = vi.fn();
    const unregister = registerDismissable(
      DISMISS_PRIORITY.contextMenu,
      closePortal,
    );
    try {
      portal.focus();
      expect(document.activeElement).toBe(portal);
      fireEvent.keyDown(portal, { key: "Tab" });
      expect(document.activeElement).toBe(portal);
      fireEvent.keyDown(portal, { key: "Escape" });
      expect(closePortal).toHaveBeenCalledTimes(1);
      expect(onClose).not.toHaveBeenCalled();
      fireEvent.mouseDown(container.firstElementChild!);
      expect(onClose).not.toHaveBeenCalled();
      unregister();
      fireEvent.mouseDown(container.firstElementChild!);
      expect(onClose).toHaveBeenCalledTimes(1);
    } finally {
      unregister();
      portal.remove();
    }
  });

  it("resets deep-linked tabs per open and restores focus on StrictMode unmount", () => {
    const onClose = vi.fn();
    const { rerender, unmount } = render(
      <StrictMode>
        <SettingsSheet open initialTab="themes" onClose={onClose} />
      </StrictMode>,
    );
    expect(
      screen
        .getByRole("button", { name: "Themes" })
        .getAttribute("aria-current"),
    ).toBe("page");
    rerender(
      <StrictMode>
        <SettingsSheet open={false} onClose={onClose} />
      </StrictMode>,
    );
    rerender(
      <StrictMode>
        <SettingsSheet open onClose={onClose} />
      </StrictMode>,
    );
    expect(
      screen
        .getByRole("button", { name: "Playback" })
        .getAttribute("aria-current"),
    ).toBe("page");
    unmount();
    expect(document.activeElement).toBe(trigger);
  });
});
