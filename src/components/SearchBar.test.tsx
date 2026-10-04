import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
} from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Provider, createStore } from "jotai";
import { getSuggestions } from "../api/tidal";
import { currentViewAtom } from "../atoms/navigation";
import type { SuggestionsResponse } from "../types";
import SearchBar from "./SearchBar";

const navigateToSearch = vi.hoisted(() => vi.fn());
vi.mock("../api/tidal", () => ({ getSuggestions: vi.fn() }));
vi.mock("../hooks/useNavigation", () => ({
  useNavigation: () => ({ navigateToSearch }),
}));
vi.mock("../hooks/usePlaybackActions", () => ({
  usePlaybackActions: () => ({}),
}));
vi.mock("./TidalImage", () => ({ default: () => null }));
vi.mock("./TrackContextMenu", () => ({ default: () => null }));
vi.mock("./MediaContextMenu", () => ({ default: () => null }));

function deferred() {
  let resolve!: (value: SuggestionsResponse) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<SuggestionsResponse>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

const suggestions = (query: string): SuggestionsResponse => ({
  textSuggestions: [{ query, source: "autocomplete" }],
  directHits: [],
});

function setup() {
  const store = createStore();
  const view = render(
    <Provider store={store}>
      <SearchBar />
    </Provider>,
  );
  const input = screen.getByPlaceholderText("Search");
  return { ...view, input, store };
}

async function type(input: HTMLElement, value: string) {
  fireEvent.change(input, { target: { value } });
  await act(async () => {
    vi.advanceTimersByTime(300);
  });
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.mocked(getSuggestions).mockReset();
  navigateToSearch.mockClear();
  localStorage.clear();
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("search suggestion request generations", () => {
  it("waits 300ms and cancels the previous debounce on another keystroke", async () => {
    vi.mocked(getSuggestions).mockResolvedValue(suggestions("latest result"));
    const { input } = setup();
    fireEvent.change(input, { target: { value: "old" } });
    act(() => vi.advanceTimersByTime(299));
    expect(getSuggestions).not.toHaveBeenCalled();
    await type(input, "latest");
    expect(getSuggestions).toHaveBeenCalledExactlyOnceWith("latest", 10);
  });

  it("ignores an older success while keeping the newer request loading", async () => {
    const old = deferred();
    const fresh = deferred();
    vi.mocked(getSuggestions)
      .mockReturnValueOnce(old.promise)
      .mockReturnValueOnce(fresh.promise);
    const { input } = setup();
    await type(input, "old");
    await type(input, "fresh");
    await act(async () => old.resolve(suggestions("old result")));
    expect(screen.queryByText("old result")).toBeNull();
    expect(screen.getByRole("status", { name: "Searching" })).toBeTruthy();
    await act(async () => fresh.resolve(suggestions("fresh result")));
    expect(screen.getByText("fresh result")).toBeTruthy();
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("does not let an older failure erase the latest completed response", async () => {
    const old = deferred();
    vi.mocked(getSuggestions)
      .mockReturnValueOnce(old.promise)
      .mockResolvedValueOnce(suggestions("fresh result"));
    const { input } = setup();
    await type(input, "old");
    await type(input, "fresh");
    await act(async () => old.reject(new Error("offline")));
    expect(screen.getByText("fresh result")).toBeTruthy();
  });

  it("clearing invalidates an in-flight response before a new query starts", async () => {
    const old = deferred();
    const fresh = deferred();
    vi.mocked(getSuggestions)
      .mockReturnValueOnce(old.promise)
      .mockReturnValueOnce(fresh.promise);
    const { input } = setup();
    await type(input, "old");
    fireEvent.click(screen.getByRole("button", { name: "Clear search" }));
    await type(input, "fresh");
    await act(async () => old.resolve(suggestions("old result")));
    expect(screen.queryByText("old result")).toBeNull();
    expect(screen.getByRole("status")).toBeTruthy();
    await act(async () => fresh.resolve(suggestions("fresh result")));
    expect(screen.getByText("fresh result")).toBeTruthy();
  });

  it.each(["clear", "enter", "unmount", "navigation"])(
    "cancels a pending debounce on %s",
    async (action) => {
      const { input, unmount, store } = setup();
      fireEvent.change(input, { target: { value: "pending" } });
      if (action === "clear")
        fireEvent.click(screen.getByRole("button", { name: "Clear search" }));
      if (action === "enter") fireEvent.keyDown(input, { key: "Enter" });
      if (action === "unmount") unmount();
      if (action === "navigation")
        act(() =>
          store.set(currentViewAtom, { type: "search", query: "another page" }),
        );
      await act(async () => vi.advanceTimersByTime(300));
      expect(getSuggestions).not.toHaveBeenCalled();
      if (action === "enter")
        expect(navigateToSearch).toHaveBeenCalledWith("pending");
    },
  );

  it("submission ignores in-flight suggestions when the input is focused again", async () => {
    const old = deferred();
    vi.mocked(getSuggestions).mockReturnValue(old.promise);
    const { input } = setup();
    await type(input, "submitted");
    fireEvent.keyDown(input, { key: "Enter" });
    await act(async () => old.resolve(suggestions("stale result")));
    fireEvent.focus(input);
    expect(screen.queryByText("stale result")).toBeNull();
    expect(screen.queryByRole("status")).toBeNull();
  });
});
