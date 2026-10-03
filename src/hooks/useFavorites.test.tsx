import { act, cleanup, renderHook } from "@testing-library/react";
import { Provider, createStore } from "jotai";
import type { PropsWithChildren } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { authTokensAtom } from "../atoms/auth";
import {
  favoriteAlbumIdsAtom,
  favoriteMixIdsAtom,
  favoritePlaylistUuidsAtom,
  favoriteTrackIdsAtom,
  favoriteVideoIdsAtom,
  followedArtistIdsAtom,
} from "../atoms/favorites";
import type { AuthTokens } from "../types";
import { useFavoriteActions, useFavorites } from "./useFavorites";

const invoke = vi.hoisted(() => vi.fn());
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

function setup() {
  const store = createStore();
  store.set(authTokensAtom, { user_id: 42 } as AuthTokens);
  const wrapper = ({ children }: PropsWithChildren) => (
    <Provider store={store}>{children}</Provider>
  );
  return { store, wrapper };
}

beforeEach(() => {
  invoke.mockReset().mockResolvedValue(undefined);
  vi.spyOn(console, "error").mockImplementation(() => {});
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("favorite actions without collection subscriptions", () => {
  it("does not rerender an action-only caller when any favorite set changes", () => {
    const { store, wrapper } = setup();
    const { result } = renderHook(() => useFavoriteActions(), { wrapper });
    const before = result.current;

    act(() => {
      store.set(favoriteTrackIdsAtom, new Set([1]));
      store.set(favoriteVideoIdsAtom, new Set([2]));
      store.set(favoriteAlbumIdsAtom, new Set([3]));
      store.set(favoritePlaylistUuidsAtom, new Set(["playlist"]));
      store.set(followedArtistIdsAtom, new Set([4]));
      store.set(favoriteMixIdsAtom, new Set(["mix"]));
    });

    expect(result.current).toBe(before);
  });

  it("preserves the collection API for existing callers", () => {
    const { store, wrapper } = setup();
    const { result } = renderHook(() => useFavorites(), { wrapper });
    act(() => store.set(favoriteAlbumIdsAtom, new Set([17])));
    expect(result.current.favoriteAlbumIds.has(17)).toBe(true);
    expect(result.current.addFavoriteTrack).toBeTypeOf("function");
    expect(result.current.removeFavoriteVideo).toBeTypeOf("function");
  });

  it.each(["track", "video"] as const)(
    "rolls back a failed optimistic %s addition",
    async (kind) => {
      const { store, wrapper } = setup();
      const { result } = renderHook(() => useFavoriteActions(), { wrapper });
      const idsAtom =
        kind === "track" ? favoriteTrackIdsAtom : favoriteVideoIdsAtom;
      let reject!: (error: Error) => void;
      invoke.mockReturnValueOnce(new Promise((_, fail) => (reject = fail)));
      let attempt!: Promise<void>;
      act(() => {
        attempt =
          kind === "track"
            ? result.current.addFavoriteTrack(7)
            : result.current.addFavoriteVideo(7);
      });
      expect(store.get(idsAtom).has(7)).toBe(true);
      const rejected = expect(attempt).rejects.toThrow("Request failed");
      await act(async () => {
        reject(new Error("Request failed"));
        await rejected;
      });
      expect(store.get(idsAtom).has(7)).toBe(false);
      expect(invoke).toHaveBeenCalledWith(`add_favorite_${kind}`, {
        userId: 42,
        [kind === "track" ? "trackId" : "videoId"]: 7,
      });
    },
  );

  it.each(["track", "video"] as const)(
    "rolls back a failed optimistic %s removal",
    async (kind) => {
      const { store, wrapper } = setup();
      const idsAtom =
        kind === "track" ? favoriteTrackIdsAtom : favoriteVideoIdsAtom;
      store.set(idsAtom, new Set([7]));
      const { result } = renderHook(() => useFavoriteActions(), { wrapper });
      let reject!: (error: Error) => void;
      invoke.mockReturnValueOnce(new Promise((_, fail) => (reject = fail)));
      let attempt!: Promise<void>;
      act(() => {
        attempt =
          kind === "track"
            ? result.current.removeFavoriteTrack(7)
            : result.current.removeFavoriteVideo(7);
      });
      expect(store.get(idsAtom).has(7)).toBe(false);
      const rejected = expect(attempt).rejects.toThrow("Request failed");
      await act(async () => {
        reject(new Error("Request failed"));
        await rejected;
      });
      expect(store.get(idsAtom).has(7)).toBe(true);
    },
  );
});
