import { memo } from "react";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
} from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ToastProvider, useToast } from "./ToastContext";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("toast render isolation", () => {
  it("shows, dismisses, and expires toasts without rendering action consumers", () => {
    vi.useFakeTimers();
    const renderRow = vi.fn();
    const onAction = vi.fn();
    const Row = memo(function Row({ id }: { id: number }) {
      renderRow(id);
      const { showToast } = useToast();
      return (
        <button
          onClick={() =>
            showToast(`Added ${id}`, "success", 3000, {
              label: `Undo ${id}`,
              onClick: onAction,
            })
          }
        >
          Queue {id}
        </button>
      );
    });
    render(
      <ToastProvider>
        {Array.from({ length: 100 }, (_, id) => (
          <Row key={id} id={id} />
        ))}
      </ToastProvider>,
    );
    renderRow.mockClear();

    fireEvent.click(screen.getByRole("button", { name: "Queue 0" }));
    expect(screen.getByText("Added 0")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Queue 1" }));
    expect(screen.getByText("Added 1")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Undo 0" }));
    expect(onAction).toHaveBeenCalledTimes(1);
    expect(screen.queryByText("Added 0")).toBeNull();
    expect(screen.getByText("Added 1")).toBeTruthy();

    act(() => vi.advanceTimersByTime(3000));
    expect(screen.queryByText("Added 1")).toBeNull();
    expect(renderRow).not.toHaveBeenCalled();
  });
});
