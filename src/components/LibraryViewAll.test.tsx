import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { Provider, createStore } from "jotai";
import type { PropsWithChildren } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../hooks/useAuth", () => ({
  useAuth: () => ({ authTokens: { user_id: 7 } }),
}));
vi.mock("../hooks/useNavigation", () => ({ useNavigation: () => ({}) }));
vi.mock("../hooks/useMediaPlay", () => ({ useMediaPlay: () => vi.fn() }));
vi.mock("../hooks/useFavorites", () => ({
  useFavorites: () => ({
    favoriteAlbumIds: new Set(),
    favoritePlaylistUuids: new Set(),
    followedArtistIds: new Set(),
    favoriteMixIds: new Set(),
  }),
}));
vi.mock("../api/tidal", () => ({
  getFavoriteMixes: vi.fn(),
  getFavoriteAlbums: vi.fn(),
  getFavoriteArtists: vi.fn(),
  getPlaylistFolders: vi.fn(),
  normalizePlaylistFolders: vi.fn(),
  getItemKey: vi.fn(),
  getFlattenedPlaylists: vi.fn(),
}));
vi.mock("./MediaCard", () => ({
  default: ({ item }: { item: { title: string } }) => <div>{item.title}</div>,
}));
vi.mock("./MediaGrid", () => ({
  default: ({ children }: PropsWithChildren) => <div>{children}</div>,
  MediaGridSkeleton: () => <div>Loading</div>,
  MediaGridEmpty: ({ message }: { message: string }) => <div>{message}</div>,
}));
vi.mock("./MediaContextMenu", () => ({ default: () => null }));
vi.mock("./FolderContextMenu", () => ({ default: () => null }));
vi.mock("./SortDropdown", () => ({ default: () => null }));

import { getFavoriteAlbums, getFavoriteMixes } from "../api/tidal";
import { getRestoreLoader } from "../hooks/useRestoreLoader";
import LibraryViewAll from "./LibraryViewAll";

const observers = new Set<IntersectionObserverCallback>();

function renderLibrary(libraryType: "mixes" | "albums" = "mixes") {
  return render(
    <Provider store={createStore()}>
      <LibraryViewAll libraryType={libraryType} />
    </Provider>,
  );
}

async function reachEnd() {
  // Text can commit before the passive effect observes the pagination sentinel.
  // Wait for the browser subscription before simulating its notification.
  if (getRestoreLoader()?.hasMore) {
    await waitFor(() => expect(observers.size).toBeGreaterThan(0));
  }
  await act(async () => {
    for (const callback of [...observers]) {
      callback(
        [{ isIntersecting: true } as IntersectionObserverEntry],
        {} as IntersectionObserver,
      );
    }
  });
}

function mix(id: string) {
  return { id, title: id, subTitle: "" };
}

describe("LibraryViewAll pagination", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    localStorage.clear();
    vi.stubGlobal(
      "IntersectionObserver",
      class {
        constructor(private callback: IntersectionObserverCallback) {}
        observe() {
          observers.add(this.callback);
        }
        disconnect() {
          observers.delete(this.callback);
        }
      },
    );
  });

  afterEach(() => {
    cleanup();
    observers.clear();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("keeps scrolling through favorites after prepended personalized mixes", async () => {
    vi.mocked(getFavoriteMixes)
      .mockResolvedValueOnce({
        items: [
          mix("Personalized A"),
          mix("Personalized B"),
          mix("Favorite 0"),
        ],
        totalNumberOfItems: 4,
        offset: 0,
        limit: 50,
        nextOffset: 1,
      })
      .mockResolvedValueOnce({
        items: [mix("Favorite 1")],
        totalNumberOfItems: 3,
        offset: 1,
        limit: 50,
      })
      .mockResolvedValueOnce({
        items: [mix("Favorite 2")],
        totalNumberOfItems: 3,
        offset: 2,
        limit: 50,
      });

    renderLibrary();
    await screen.findByText("Personalized A");
    await reachEnd();
    expect(getFavoriteMixes).toHaveBeenNthCalledWith(2, 1, 50, "DATE", "DESC");
    expect(screen.getByText("Favorite 1")).toBeTruthy();
    // Four displayed cards do not exhaust the three favorite API offsets.
    expect(getRestoreLoader()?.hasMore).toBe(true);
    await reachEnd();
    expect(getFavoriteMixes).toHaveBeenNthCalledWith(3, 2, 50, "DATE", "DESC");
    expect(screen.getByText("Favorite 2")).toBeTruthy();
    expect(getRestoreLoader()?.hasMore).toBe(false);
    await reachEnd();
    expect(getFavoriteMixes).toHaveBeenCalledTimes(3);
  });

  it("honors API offsets while search loads the remaining mix pages", async () => {
    vi.mocked(getFavoriteMixes)
      .mockResolvedValueOnce({
        items: [mix("Personalized"), mix("Favorite 0")],
        totalNumberOfItems: 6,
        offset: 0,
        limit: 50,
        nextOffset: 1,
      })
      .mockResolvedValueOnce({
        items: [mix("Favorite 1")],
        totalNumberOfItems: 6,
        offset: 1,
        limit: 50,
        nextOffset: 5,
      })
      .mockResolvedValueOnce({
        items: [mix("Favorite 5")],
        totalNumberOfItems: 6,
        offset: 5,
        limit: 50,
      });

    renderLibrary();
    await screen.findByText("Personalized");
    fireEvent.focus(screen.getByPlaceholderText("Filter by title"));
    await screen.findByText("Favorite 5");
    expect(
      vi.mocked(getFavoriteMixes).mock.calls.map(([offset]) => offset),
    ).toEqual([0, 1, 5]);
    expect(getRestoreLoader()?.hasMore).toBe(false);
  });

  it.each(["scroll", "search"] as const)(
    "stops %s pagination on an empty page with a stale total",
    async (trigger) => {
      vi.spyOn(console, "error").mockImplementation(() => {});
      vi.mocked(getFavoriteMixes)
        .mockResolvedValueOnce({
          items: [mix("Favorite 0")],
          totalNumberOfItems: 20,
          offset: 0,
          limit: 50,
        })
        .mockResolvedValueOnce({
          items: [],
          totalNumberOfItems: 20,
          offset: 1,
          limit: 50,
        })
        .mockRejectedValue(new Error("Pagination retried an exhausted page"));

      renderLibrary();
      await screen.findByText("Favorite 0");
      if (trigger === "search") {
        fireEvent.focus(screen.getByPlaceholderText("Filter by title"));
      } else {
        await reachEnd();
      }
      await waitFor(() => expect(getRestoreLoader()?.hasMore).toBe(false));
      await reachEnd();
      expect(getFavoriteMixes).toHaveBeenCalledTimes(2);
    },
  );

  it("uses item counts for ordinary album pages without explicit offsets", async () => {
    vi.mocked(getFavoriteAlbums)
      .mockResolvedValueOnce({
        items: [
          { id: 10, title: "Album A" },
          { id: 11, title: "Album B" },
        ],
        totalNumberOfItems: 3,
        offset: 0,
        limit: 50,
      } as Awaited<ReturnType<typeof getFavoriteAlbums>>)
      .mockResolvedValueOnce({
        items: [{ id: 12, title: "Album C" }],
        totalNumberOfItems: 3,
        offset: 2,
        limit: 50,
      } as Awaited<ReturnType<typeof getFavoriteAlbums>>);

    renderLibrary("albums");
    await screen.findByText("Album A");
    await reachEnd();
    expect(getFavoriteAlbums).toHaveBeenNthCalledWith(
      2,
      7,
      2,
      50,
      "DATE",
      "DESC",
    );
    expect(screen.getByText("Album C")).toBeTruthy();
    expect(getRestoreLoader()?.hasMore).toBe(false);
  });
});
