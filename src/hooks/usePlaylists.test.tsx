import { act, cleanup, renderHook } from "@testing-library/react";
import { Provider, createStore } from "jotai";
import type { PropsWithChildren } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { userPlaylistsAtom } from "../atoms/playlists";
import type { Playlist } from "../types";
import { usePlaylists } from "./usePlaylists";

const { invoke, invalidateCache } = vi.hoisted(() => ({
  invoke: vi.fn(),
  invalidateCache: vi.fn(),
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));
vi.mock("../api/tidal", () => ({
  invalidateCache,
  getPlaylistFolders: vi.fn().mockResolvedValue({ items: [] }),
  normalizePlaylistFolders: vi.fn((value) => value),
}));

function setup() {
  const store = createStore();
  store.set(userPlaylistsAtom, [
    { uuid: "playlist", numberOfTracks: 3 } as Playlist,
  ]);
  const wrapper = ({ children }: PropsWithChildren) => (
    <Provider store={store}>{children}</Provider>
  );
  return { store, ...renderHook(() => usePlaylists(), { wrapper }) };
}

beforeEach(() => {
  invoke.mockReset().mockResolvedValue(undefined);
  invalidateCache.mockClear();
  vi.spyOn(console, "error").mockImplementation(() => {});
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("playlist item removal", () => {
  it("passes the occurrence ID so duplicate recordings can be removed individually", async () => {
    const { store, result } = setup();

    await act(async () => {
      await result.current.removeTrackFromPlaylist("playlist", 1, {
        playlistItemId: "second-occurrence",
        resourceId: 42,
        resourceType: "tracks",
      });
    });

    expect(invoke).toHaveBeenCalledExactlyOnceWith(
      "remove_track_from_playlist",
      {
        playlistId: "playlist",
        index: 1,
        playlistItemId: "second-occurrence",
        resourceId: "42",
        resourceType: "tracks",
      },
    );
    expect(store.get(userPlaylistsAtom)[0].numberOfTracks).toBe(2);
    expect(invalidateCache).toHaveBeenCalledWith("playlist:playlist");
    expect(invalidateCache).toHaveBeenCalledWith("playlist-page:playlist");
  });

  it("uses the legacy row index when no official item ID is available", async () => {
    const { result } = setup();

    await act(async () => {
      await result.current.removeTrackFromPlaylist("playlist", 1, {
        resourceId: 42,
        resourceType: "tracks",
      });
    });

    expect(invoke).toHaveBeenCalledExactlyOnceWith(
      "remove_track_from_playlist",
      {
        playlistId: "playlist",
        index: 1,
        playlistItemId: null,
        resourceId: null,
        resourceType: null,
      },
    );
  });

  it("restores the count on a failed deletion without retrying the mutation", async () => {
    const { store, result } = setup();
    const error = new Error("Deletion response lost");
    invoke.mockRejectedValueOnce(error);

    await act(async () => {
      await expect(
        result.current.removeTrackFromPlaylist("playlist", 1, {
          playlistItemId: "second-occurrence",
          resourceId: 42,
          resourceType: "tracks",
        }),
      ).rejects.toBe(error);
    });

    expect(invoke).toHaveBeenCalledTimes(1);
    expect(store.get(userPlaylistsAtom)[0].numberOfTracks).toBe(3);
    expect(invalidateCache).not.toHaveBeenCalledWith("playlist-page:playlist");
  });
});
