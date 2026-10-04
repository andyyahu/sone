import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
} from "@testing-library/react";
import { Provider, createStore } from "jotai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  favoriteAlbumIdsAtom,
  favoriteTrackIdsAtom,
  favoriteVideoIdsAtom,
  followedArtistIdsAtom,
} from "../atoms/favorites";
import { authTokensAtom } from "../atoms/auth";
import { currentViewAtom } from "../atoms/navigation";
import type { AuthTokens, Track } from "../types";

const { imageRender, invoke } = vi.hoisted(() => ({
  imageRender: vi.fn(),
  invoke: vi.fn(),
}));
vi.mock("./TidalImage", () => ({
  default: ({ alt }: { alt: string }) => {
    imageRender(alt);
    return null;
  },
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));
vi.mock("../contexts/ToastContext", () => ({
  useToast: () => ({ showToast: vi.fn() }),
}));

import TrackList from "./TrackList";

function track(id: number, title: string, itemType?: "video"): Track {
  return {
    id,
    title,
    itemType,
    duration: 180,
    artists: [],
    album: { id: 10, title, cover: "test-cover" },
  } as Track;
}

beforeEach(() => {
  imageRender.mockClear();
  invoke.mockReset().mockResolvedValue(undefined);
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("track row favorite subscriptions", () => {
  it("does not render rows for unrelated collection or track changes", () => {
    const store = createStore();
    render(
      <Provider store={store}>
        <TrackList
          tracks={[track(1, "First"), track(2, "Second")]}
          onPlay={vi.fn()}
        />
      </Provider>,
    );
    imageRender.mockClear();

    act(() => {
      store.set(favoriteAlbumIdsAtom, new Set([10]));
      store.set(followedArtistIdsAtom, new Set([20]));
      store.set(favoriteVideoIdsAtom, new Set([1]));
      store.set(favoriteTrackIdsAtom, new Set([999]));
    });

    expect(imageRender).not.toHaveBeenCalled();
    expect(screen.getAllByTitle("Add to favorites")).toHaveLength(2);
  });

  it("updates only the matching row and keeps audio and video favorites separate", () => {
    const store = createStore();
    render(
      <Provider store={store}>
        <TrackList
          tracks={[
            track(1, "Audio"),
            track(1, "Video", "video"),
            track(2, "Other"),
          ]}
          onPlay={vi.fn()}
        />
      </Provider>,
    );
    imageRender.mockClear();

    act(() => store.set(favoriteTrackIdsAtom, new Set([1])));
    expect(imageRender.mock.calls).toEqual([["Audio"]]);
    expect(screen.getAllByTitle("Remove from favorites")).toHaveLength(1);
    imageRender.mockClear();

    act(() => store.set(favoriteVideoIdsAtom, new Set([1])));
    expect(imageRender.mock.calls).toEqual([["Video"]]);
    expect(screen.getAllByTitle("Remove from favorites")).toHaveLength(2);
  });

  it("shares one auth subscription and does not redraw rows on token refresh", () => {
    const store = createStore();
    store.set(authTokensAtom, { user_id: 42 } as AuthTokens);
    const subscribe = vi.spyOn(store, "sub");
    render(
      <Provider store={store}>
        <TrackList
          tracks={Array.from({ length: 20 }, (_, index) =>
            track(index, `Track ${index}`),
          )}
          onPlay={vi.fn()}
        />
      </Provider>,
    );

    expect(
      subscribe.mock.calls.filter(([atom]) => atom === authTokensAtom),
    ).toHaveLength(1);
    imageRender.mockClear();
    act(() => {
      store.set(authTokensAtom, {
        user_id: 42,
        access_token: "refreshed-token",
      } as AuthTokens);
    });
    expect(imageRender).not.toHaveBeenCalled();
  });

  it("uses current account actions for both audio and video after account changes", async () => {
    const store = createStore();
    store.set(authTokensAtom, { user_id: 42 } as AuthTokens);
    render(
      <Provider store={store}>
        <TrackList
          tracks={[track(1, "Audio"), track(2, "Video", "video")]}
          onPlay={vi.fn()}
        />
      </Provider>,
    );

    act(() => store.set(authTokensAtom, { user_id: 43 } as AuthTokens));
    await act(async () => {
      screen.getAllByTitle("Add to favorites").forEach((button) => {
        fireEvent.click(button);
      });
    });
    expect(invoke).toHaveBeenCalledWith("add_favorite_track", {
      userId: 43,
      trackId: 1,
    });
    expect(invoke).toHaveBeenCalledWith("add_favorite_video", {
      userId: 43,
      videoId: 2,
    });

    act(() => store.set(authTokensAtom, { user_id: 44 } as AuthTokens));
    await act(async () => {
      screen.getAllByTitle("Remove from favorites").forEach((button) => {
        fireEvent.click(button);
      });
    });
    expect(invoke).toHaveBeenCalledWith("remove_favorite_track", {
      userId: 44,
      trackId: 1,
    });
    expect(invoke).toHaveBeenCalledWith("remove_favorite_video", {
      userId: 44,
      videoId: 2,
    });
  });

  it("retains album navigation without triggering row playback", () => {
    const store = createStore();
    const onPlay = vi.fn();
    const item = track(1, "Song");
    item.album = { id: 10, title: "Selected Album", cover: "test-cover" };
    render(
      <Provider store={store}>
        <TrackList tracks={[item]} onPlay={onPlay} />
      </Provider>,
    );

    fireEvent.click(screen.getByText("Selected Album"));
    expect(store.get(currentViewAtom)).toMatchObject({
      type: "album",
      albumId: 10,
      albumInfo: { title: "Selected Album", cover: "test-cover" },
    });
    expect(onPlay).not.toHaveBeenCalled();
  });
});
