import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
} from "@testing-library/react";
import { Provider, createStore } from "jotai";
import { StrictMode, type ComponentProps } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { authTokensAtom } from "../atoms/auth";
import { getRestoreLoader } from "../hooks/useRestoreLoader";
import type { AuthTokens, Playlist, Track } from "../types";

const mocks = vi.hoisted(() => ({
  tracks: vi.fn(),
  details: vi.fn(),
  recommendations: vi.fn(),
  playFromSource: vi.fn().mockResolvedValue(undefined),
  updatePlaylist: vi.fn().mockResolvedValue(undefined),
  addTrackToPlaylist: vi.fn(),
  showToast: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("../api/tidal", () => ({
  getPlaylistTracksPage: mocks.tracks,
  getPlaylistDetails: mocks.details,
  getPlaylistRecommendations: mocks.recommendations,
  invalidateCache: vi.fn(),
}));
vi.mock("../hooks/usePlaybackActions", () => ({
  usePlaybackActions: () => mocks,
}));
vi.mock("../hooks/usePlaylists", () => ({
  usePlaylists: () => ({ ...mocks, userPlaylists: [] }),
}));
vi.mock("../hooks/useFavorites", () => ({
  useFavorites: () => ({ favoritePlaylistUuids: new Set() }),
}));
vi.mock("../contexts/ToastContext", () => ({
  useToast: () => mocks,
}));
vi.mock("./TidalImage", () => ({
  default: ({ src, alt }: { src: string; alt: string }) => (
    <img src={src} alt={alt} />
  ),
}));
vi.mock("./CoverBanner", () => ({ default: () => null }));
vi.mock("./MediaContextMenu", () => ({ default: () => null }));
vi.mock("./SourcePlayButton", () => ({ default: () => null }));
vi.mock("./TrackList", () => ({
  default: ({
    tracks,
    onPlay,
    onLoadMore,
    hasMore,
    sortLoading,
  }: ComponentProps<typeof import("./TrackList").default>) => (
    <div>
      {sortLoading ? (
        <div role="status">Loading songs</div>
      ) : (
        tracks.map((track, index) => (
          <button key={track.id} onClick={() => onPlay(track, index)}>
            {track.title}
          </button>
        ))
      )}
      {hasMore && <button onClick={onLoadMore}>Load more songs</button>}
    </div>
  ),
}));
vi.mock("./AddToPlaylistMenu", () => ({
  EditPlaylistModal: ({
    onUpdated,
  }: {
    onUpdated: (playlist: Playlist) => void;
  }) => (
    <button
      onClick={() =>
        onUpdated({
          uuid: "first",
          title: "Edited title",
          description: "Edited description",
          accessType: "UNLISTED",
        })
      }
    >
      Save edit
    </button>
  ),
}));

import PlaylistView from "./PlaylistView";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

function track(id: number): Track {
  return { id, title: `Song ${id}`, duration: 180 } as Track;
}

function details(uuid: string, title: string): Playlist {
  return {
    uuid,
    title,
    creator: { id: 7, name: "Owner" },
    accessType: "PUBLIC",
    duration: 360,
  };
}

function setup(
  info: ComponentProps<typeof PlaylistView>["playlistInfo"] | null = {
    title: "Known title",
    image: "known-cover",
  },
) {
  const store = createStore();
  store.set(authTokensAtom, { user_id: 7 } as AuthTokens);
  const onBack = vi.fn();
  const view = (playlistId: string, playlistInfo = info ?? undefined) => (
    <Provider store={store}>
      <PlaylistView {...{ playlistId, playlistInfo, onBack }} />
    </Provider>
  );
  return { ...render(view("first")), view, store };
}

beforeEach(() => {
  vi.clearAllMocks();
  mocks.tracks.mockReturnValue(new Promise(() => {}));
  mocks.details.mockReturnValue(new Promise(() => {}));
  mocks.recommendations.mockResolvedValue({ items: [] });
});

afterEach(cleanup);

describe("playlist initial loading", () => {
  it("shows the known header immediately, then songs before metadata or later pages", async () => {
    const firstPage = deferred<{
      items: Track[];
      totalNumberOfItems: number;
    }>();
    mocks.tracks.mockReturnValueOnce(firstPage.promise);
    const { rerender, view } = setup();

    expect(screen.getByRole("heading", { name: "Known title" })).toBeTruthy();
    expect(
      screen.getByRole("img", { name: "Known title" }).getAttribute("src"),
    ).toContain("known/cover");
    expect(screen.getByRole("status").textContent).toBe("Loading songs");
    expect(screen.queryByText("This playlist is empty")).toBeNull();
    expect(mocks.tracks).toHaveBeenCalledTimes(1);
    expect(getRestoreLoader()).toBeNull();
    expect(mocks.details).not.toHaveBeenCalled();
    expect(mocks.recommendations).not.toHaveBeenCalled();

    await act(async () => {
      firstPage.resolve({ items: [track(1)], totalNumberOfItems: 300 });
    });

    fireEvent.click(screen.getByRole("button", { name: "Song 1" }));
    expect(mocks.playFromSource).toHaveBeenCalledWith(
      track(1),
      [track(1)],
      expect.anything(),
    );
    expect(mocks.details).toHaveBeenCalledTimes(1);
    expect(getRestoreLoader()?.hasMore).toBe(true);
    expect(mocks.tracks.mock.invocationCallOrder[0]).toBeLessThan(
      mocks.details.mock.invocationCallOrder[0],
    );
    expect(mocks.recommendations).not.toHaveBeenCalled();

    // An equivalent navigation hint must not enqueue another details request.
    rerender(view("first", { title: "Known title", image: "known-cover" }));
    expect(mocks.details).toHaveBeenCalledTimes(1);
  });

  it("waits for the last song page before requesting recommendations", async () => {
    const lastPage = deferred<{ items: Track[]; totalNumberOfItems: number }>();
    mocks.tracks
      .mockResolvedValueOnce({ items: [track(1)], totalNumberOfItems: 2 })
      .mockReturnValueOnce(lastPage.promise);
    setup();
    await screen.findByText("Song 1");
    expect(mocks.recommendations).not.toHaveBeenCalled();
    fireEvent.click(screen.getByText("Load more songs"));
    expect(mocks.recommendations).not.toHaveBeenCalled();

    await act(async () => {
      lastPage.resolve({ items: [track(2)], totalNumberOfItems: 2 });
    });
    expect(screen.getByText("Song 2")).toBeTruthy();
    expect(mocks.recommendations).toHaveBeenCalledExactlyOnceWith(
      "first",
      0,
      50,
    );
  });

  it("does not duplicate the deferred metadata request in Strict Mode", async () => {
    mocks.tracks.mockResolvedValue({
      items: [track(1)],
      totalNumberOfItems: 2,
    });
    render(
      <StrictMode>
        <Provider>
          <PlaylistView
            playlistId="first"
            playlistInfo={{ title: "Known title" }}
            onBack={vi.fn()}
          />
        </Provider>
      </StrictMode>,
    );
    await screen.findByText("Song 1");
    expect(mocks.details).toHaveBeenCalledExactlyOnceWith("first");
  });

  it("uses a placeholder for deep links without holding songs for metadata", async () => {
    const firstPage = deferred<{
      items: Track[];
      totalNumberOfItems: number;
    }>();
    const metadata = deferred<Playlist>();
    mocks.tracks.mockReturnValue(firstPage.promise);
    mocks.details.mockReturnValue(metadata.promise);
    setup(null);
    expect(screen.queryByRole("heading")).toBeNull();
    expect(mocks.details).not.toHaveBeenCalled();
    await act(async () => {
      firstPage.resolve({ items: [track(1)], totalNumberOfItems: 2 });
    });
    expect(screen.getByText("Song 1")).toBeTruthy();
    await act(async () => {
      metadata.resolve(details("first", "Resolved title"));
    });
    expect(
      screen.getByRole("heading", { name: "Resolved title" }),
    ).toBeTruthy();
    expect(screen.getByRole("button", { name: "Edit playlist" })).toBeTruthy();
    expect(
      screen.getByRole("button", { name: "Make playlist private" }),
    ).toBeTruthy();
    expect(screen.getByText("(6:00)")).toBeTruthy();
  });

  it("ignores metadata and recommendation responses after changing playlists", async () => {
    const oldDetails = deferred<Playlist>();
    const oldRecommendations = deferred<{ items: Track[] }>();
    mocks.tracks
      .mockResolvedValueOnce({ items: [track(1)], totalNumberOfItems: 1 })
      .mockResolvedValueOnce({ items: [track(2)], totalNumberOfItems: 1 });
    mocks.details.mockReturnValueOnce(oldDetails.promise);
    mocks.recommendations.mockReturnValueOnce(oldRecommendations.promise);
    const { rerender, view } = setup();
    await screen.findByText("Song 1");
    rerender(view("second", { title: "Second playlist" }));
    await screen.findByText("Song 2");
    await act(async () => {
      oldDetails.resolve(details("first", "Old title"));
      oldRecommendations.resolve({ items: [track(90)] });
    });
    expect(
      screen.getByRole("heading", { name: "Second playlist" }),
    ).toBeTruthy();
    expect(screen.queryByText("Old title")).toBeNull();
    expect(screen.queryByText("Song 90")).toBeNull();
    expect(screen.queryByRole("button", { name: "Edit playlist" })).toBeNull();
  });

  it("ignores a previous playlist's first songs after navigating away", async () => {
    const oldPage = deferred<{ items: Track[]; totalNumberOfItems: number }>();
    mocks.tracks
      .mockReturnValueOnce(oldPage.promise)
      .mockResolvedValueOnce({ items: [track(2)], totalNumberOfItems: 2 });
    const { rerender, view } = setup();
    rerender(view("second", { title: "Second playlist" }));
    await screen.findByText("Song 2");
    await act(async () => {
      oldPage.resolve({ items: [track(1)], totalNumberOfItems: 100 });
    });
    expect(screen.queryByText("Song 1")).toBeNull();
    expect(screen.getByText("Song 2")).toBeTruthy();
    expect(mocks.details).toHaveBeenCalledExactlyOnceWith("second");
  });

  it("ignores another account's pending metadata when the playlist stays the same", async () => {
    const oldDetails = deferred<Playlist>();
    mocks.tracks.mockResolvedValue({
      items: [track(1)],
      totalNumberOfItems: 2,
    });
    mocks.details
      .mockReturnValueOnce(oldDetails.promise)
      .mockResolvedValueOnce(details("first", "Resolved title"));
    const { store } = setup();
    await screen.findByText("Song 1");
    await act(async () => {
      store.set(authTokensAtom, { user_id: 8 } as AuthTokens);
    });
    await screen.findByRole("heading", { name: "Resolved title" });
    await act(async () => {
      oldDetails.resolve(details("first", "Old account title"));
    });
    expect(screen.queryByText("Old account title")).toBeNull();
    expect(screen.queryByRole("button", { name: "Edit playlist" })).toBeNull();
    expect(mocks.details).toHaveBeenCalledTimes(2);
  });

  it("preserves edits made while metadata is pending and still fills in duration", async () => {
    const metadata = deferred<Playlist>();
    mocks.tracks.mockResolvedValue({
      items: [track(1)],
      totalNumberOfItems: 2,
    });
    mocks.details.mockReturnValue(metadata.promise);
    setup({ title: "Known title", image: "known-cover", isUserPlaylist: true });
    await screen.findByText("Song 1");
    fireEvent.click(screen.getByRole("button", { name: "Edit playlist" }));
    fireEvent.click(screen.getByText("Save edit"));
    await act(async () => {
      metadata.resolve(details("first", "Old server title"));
    });
    expect(screen.getByRole("heading", { name: "Edited title" })).toBeTruthy();
    expect(screen.getByText("Edited description")).toBeTruthy();
    expect(
      screen.getByRole("img", { name: "Edited title" }).getAttribute("src"),
    ).toContain("known/cover");
    expect(
      screen.getByRole("button", { name: "Make playlist public" }),
    ).toBeTruthy();
    expect(screen.getByText("(6:00)")).toBeTruthy();
  });
});
