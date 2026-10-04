import { describe, it, expect, vi, beforeEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { Provider, createStore } from "jotai";
import type { PropsWithChildren } from "react";
import { useNavigation } from "./useNavigation";
import { drawerOpenAtom, maximizedPlayerAtom } from "../atoms/ui";
import { currentViewAtom } from "../atoms/navigation";
import { scrollKey } from "../lib/scrollMemory";
import type { ProfilePlaylist } from "../types";

describe("useNavigation overlay dismissal", () => {
  beforeEach(() => {
    // Don't mutate real jsdom history between cases.
    vi.spyOn(window.history, "pushState").mockImplementation(() => {});
  });

  function setup() {
    const store = createStore();
    store.set(drawerOpenAtom, true);
    store.set(maximizedPlayerAtom, true);
    const wrapper = ({ children }: PropsWithChildren) => (
      <Provider store={store}>{children}</Provider>
    );
    const { result } = renderHook(() => useNavigation(), { wrapper });
    return { store, result };
  }

  it("closes the queue drawer and fullscreen player when navigating to an album", () => {
    const { store, result } = setup();
    act(() => {
      result.current.navigateToAlbum(123);
    });
    expect(store.get(drawerOpenAtom)).toBe(false);
    expect(store.get(maximizedPlayerAtom)).toBe(false);
    expect(store.get(currentViewAtom)).toMatchObject({
      type: "album",
      albumId: 123,
    });
  });

  it("closes overlays when navigating to an artist", () => {
    const { store, result } = setup();
    act(() => {
      result.current.navigateToArtist(7, { name: "Artist" });
    });
    expect(store.get(drawerOpenAtom)).toBe(false);
    expect(store.get(maximizedPlayerAtom)).toBe(false);
    expect(store.get(currentViewAtom)).toMatchObject({
      type: "artist",
      artistId: 7,
    });
  });
});

describe("useNavigation profile playlists", () => {
  beforeEach(() => {
    vi.spyOn(window.history, "pushState").mockImplementation(() => {});
  });

  it("navigates to the profile playlists view with the passed list and name", () => {
    const store = createStore();
    store.set(drawerOpenAtom, true);
    store.set(maximizedPlayerAtom, true);
    const wrapper = ({ children }: PropsWithChildren) => (
      <Provider store={store}>{children}</Provider>
    );
    const { result } = renderHook(() => useNavigation(), { wrapper });

    const playlists: ProfilePlaylist[] = [
      { id: "p1", title: "First" },
      { id: "p2", title: "Second" },
    ];
    act(() => {
      result.current.navigateToProfilePlaylists(playlists, "Alice");
    });

    expect(store.get(drawerOpenAtom)).toBe(false);
    expect(store.get(maximizedPlayerAtom)).toBe(false);
    expect(store.get(currentViewAtom)).toMatchObject({
      type: "profilePlaylists",
      profileName: "Alice",
      playlists,
    });
  });
});

describe("useNavigation scroll memory stamping", () => {
  it("stamps the pushed view so the entry can own a scroll offset", () => {
    const spy = vi
      .spyOn(window.history, "pushState")
      .mockImplementation(() => {});
    const store = createStore();
    const wrapper = ({ children }: PropsWithChildren) => (
      <Provider store={store}>{children}</Provider>
    );
    const { result } = renderHook(() => useNavigation(), { wrapper });

    const callsBefore = spy.mock.calls.length;
    act(() => {
      result.current.navigateToAlbum(42);
    });

    expect(scrollKey(store.get(currentViewAtom))).not.toBe(null);
    expect(spy).toHaveBeenCalledTimes(callsBefore + 1);
    const pushed = spy.mock.calls[callsBefore][0] as { __navId?: number };
    expect(pushed.__navId).toBe(store.get(currentViewAtom).__navId);
  });
});

describe("useNavigation shared actions", () => {
  beforeEach(() => {
    vi.spyOn(window.history, "pushState").mockImplementation(() => {});
  });

  it("shares stable actions across consumers and remounted rows in one store", () => {
    const store = createStore();
    const wrapper = ({ children }: PropsWithChildren) => (
      <Provider store={store}>{children}</Provider>
    );
    const first = renderHook(() => useNavigation(), { wrapper });
    const second = renderHook(() => useNavigation(), { wrapper });
    const actions = first.result.current;

    expect(second.result.current).toBe(actions);
    first.rerender();
    expect(first.result.current).toBe(actions);
    first.unmount();

    const remounted = renderHook(() => useNavigation(), { wrapper });
    expect(remounted.result.current).toBe(actions);
    expect(remounted.result.current.navigateToArtist).toBe(
      second.result.current.navigateToArtist,
    );
  });

  it("keeps navigation and overlay updates isolated between stores", () => {
    const firstStore = createStore();
    const secondStore = createStore();
    for (const store of [firstStore, secondStore]) {
      store.set(drawerOpenAtom, true);
      store.set(maximizedPlayerAtom, true);
    }
    const first = renderHook(() => useNavigation(), {
      wrapper: ({ children }: PropsWithChildren) => (
        <Provider store={firstStore}>{children}</Provider>
      ),
    });
    const second = renderHook(() => useNavigation(), {
      wrapper: ({ children }: PropsWithChildren) => (
        <Provider store={secondStore}>{children}</Provider>
      ),
    });
    expect(first.result.current).not.toBe(second.result.current);

    const playlistInfo = { title: "My playlist", numberOfTracks: 23 };
    const callsBefore = vi.mocked(window.history.pushState).mock.calls.length;
    act(() => {
      second.result.current.navigateToPlaylist("playlist-2", playlistInfo);
    });

    expect(firstStore.get(currentViewAtom)).toEqual({ type: "home" });
    expect(firstStore.get(drawerOpenAtom)).toBe(true);
    expect(firstStore.get(maximizedPlayerAtom)).toBe(true);
    expect(secondStore.get(drawerOpenAtom)).toBe(false);
    expect(secondStore.get(maximizedPlayerAtom)).toBe(false);
    const view = secondStore.get(currentViewAtom);
    expect(view).toMatchObject({
      type: "playlist",
      playlistId: "playlist-2",
      playlistInfo,
    });
    expect(window.history.pushState).toHaveBeenCalledTimes(callsBefore + 1);
    expect(window.history.pushState).toHaveBeenLastCalledWith(view, "");
    expect(scrollKey(view)).not.toBe(null);
  });

  it("uses the new store when the surrounding Provider changes stores", () => {
    const originalStore = createStore();
    const nextStore = createStore();
    let activeStore = originalStore;
    const wrapper = ({ children }: PropsWithChildren) => (
      <Provider store={activeStore}>{children}</Provider>
    );
    const { result, rerender } = renderHook(() => useNavigation(), { wrapper });
    const originalActions = result.current;
    activeStore = nextStore;
    rerender();
    expect(result.current).not.toBe(originalActions);

    act(() => {
      result.current.navigateToPlaylistFolder("folder-1", "Folder");
    });
    expect(originalStore.get(currentViewAtom)).toEqual({ type: "home" });
    expect(nextStore.get(currentViewAtom)).toMatchObject({
      type: "libraryViewAll",
      libraryType: "playlists",
      folderId: "folder-1",
      folderName: "Folder",
    });

    activeStore = originalStore;
    rerender();
    expect(result.current).toBe(originalActions);
  });
});
