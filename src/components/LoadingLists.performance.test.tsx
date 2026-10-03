import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
} from "@testing-library/react";
import { Provider, createStore } from "jotai";
import type { ComponentProps } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { authTokensAtom } from "../atoms/auth";
import { favoriteAlbumIdsAtom } from "../atoms/favorites";
import {
  folderCountAdjustmentsAtom,
  renamedFoldersAtom,
} from "../atoms/playlists";
import { PageScrollProvider } from "../contexts/PageScrollContext";
import { getRestoreLoader } from "../hooks/useRestoreLoader";
import type { AuthTokens, Track } from "../types";

const mocks = vi.hoisted(() => ({
  tracks: vi.fn(),
  metadata: vi.fn(),
  recommendations: vi.fn(),
  albums: vi.fn(),
  folders: vi.fn(),
  images: vi.fn(),
  cards: vi.fn(),
  playFromSource: vi.fn().mockResolvedValue(undefined),
  playAllFromSource: vi.fn(),
  appendToQueue: vi.fn(),
  playTrack: vi.fn(),
  setShuffledQueue: vi.fn(),
  navigateToAlbum: vi.fn(),
  navigateToPlaylistFolder: vi.fn(),
  playMedia: vi.fn(),
  showToast: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("../api/tidal", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../api/tidal")>()),
  getPlaylistTracksPage: mocks.tracks,
  getPlaylistDetails: mocks.metadata,
  getPlaylistRecommendations: mocks.recommendations,
  getFavoriteAlbums: mocks.albums,
  getPlaylistFolders: mocks.folders,
}));
vi.mock("../hooks/usePlaybackActions", () => ({
  usePlaybackActions: () => mocks,
}));
vi.mock("../hooks/useMediaPlay", () => ({
  useMediaPlay: () => mocks.playMedia,
}));
vi.mock("../hooks/useNavigation", () => ({
  useNavigation: () => mocks,
}));
vi.mock("../contexts/ToastContext", () => ({
  useToast: () => ({ showToast: mocks.showToast }),
}));
vi.mock("./TidalImage", () => ({
  default: ({ alt }: { alt: string }) => {
    mocks.images(alt);
    return null;
  },
}));
vi.mock("./CoverBanner", () => ({ default: () => null }));
vi.mock("./MediaCard", async (importOriginal) => {
  const { default: MediaCard } =
    await importOriginal<typeof import("./MediaCard")>();
  return {
    default: (props: ComponentProps<typeof MediaCard>) => {
      mocks.cards(props);
      return <MediaCard {...props} />;
    },
  };
});
vi.mock("./TrackContextMenu", () => ({
  default: ({
    index,
    onTrackRemoved,
  }: {
    index: number;
    onTrackRemoved: (index: number) => void;
  }) => (
    <button onClick={() => onTrackRemoved(index)}>Remove test track</button>
  ),
}));
vi.mock("./FolderContextMenu", () => ({
  default: ({ folderName }: { folderName: string }) => (
    <div role="dialog">{folderName}</div>
  ),
}));

import PlaylistView from "./PlaylistView";
import LibraryViewAll from "./LibraryViewAll";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

function track(id: number): Track {
  return {
    id,
    title: `Track ${id}`,
    duration: 180,
    artists: [{ id: 1, name: "Artist" }],
    album: { id: 10, title: `Cover ${id}`, cover: "cover" },
  } as Track;
}

function album(id: number) {
  return {
    id,
    title: `Album ${id}`,
    cover: "cover",
    artist: { name: "Artist" },
  };
}

function makeStore() {
  const store = createStore();
  store.set(authTokensAtom, { user_id: 7 } as AuthTokens);
  return store;
}

beforeEach(() => {
  vi.clearAllMocks();
  mocks.metadata.mockReturnValue(new Promise(() => {}));
  mocks.recommendations.mockReturnValue(new Promise(() => {}));
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  vi.stubGlobal(
    "IntersectionObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("list rendering during loading", () => {
  it("retains existing playlist rows through loading and appending, while play reads the complete queue", async () => {
    const secondPage = deferred<{
      items: Track[];
      totalNumberOfItems: number;
    }>();
    const metadata = deferred<{ duration: number }>();
    const recommendations = deferred<{ items: Track[] }>();
    const first = track(1);
    const second = track(2);
    mocks.tracks.mockResolvedValueOnce({
      items: [first],
      totalNumberOfItems: 2,
    });
    mocks.tracks.mockReturnValueOnce(secondPage.promise);
    mocks.metadata.mockReturnValue(metadata.promise);
    mocks.recommendations.mockReturnValue(recommendations.promise);

    const scroller = document.createElement("div");
    document.body.appendChild(scroller);
    Object.defineProperties(scroller, {
      clientHeight: { value: 480 },
      offsetHeight: { value: 480 },
    });
    scroller.scrollTo = vi.fn();
    const view = render(
      <Provider store={makeStore()}>
        <PageScrollProvider element={scroller}>
          <PlaylistView
            playlistId="playlist"
            playlistInfo={{ title: "Playlist", isUserPlaylist: true }}
            onBack={vi.fn()}
          />
        </PageScrollProvider>
      </Provider>,
    );
    await screen.findByText("Track 1");
    mocks.images.mockClear();

    act(() => {
      void getRestoreLoader()?.loadMore();
    });
    expect(mocks.tracks).toHaveBeenCalledTimes(2);
    expect(mocks.images).not.toHaveBeenCalled();

    await act(async () => {
      metadata.resolve({ duration: 360 });
      recommendations.resolve({ items: [] });
    });
    expect(mocks.images).not.toHaveBeenCalled();

    await act(async () => {
      secondPage.resolve({ items: [second], totalNumberOfItems: 2 });
    });
    expect(mocks.images.mock.calls).toEqual([["Cover 2"]]);
    await act(async () => {
      fireEvent.click(screen.getByText("Track 1"));
    });
    expect(mocks.playFromSource).toHaveBeenLastCalledWith(
      first,
      [first, second],
      {
        source: expect.objectContaining({ allTracks: [first, second] }),
      },
    );

    fireEvent.contextMenu(screen.getByText("Track 1"));
    fireEvent.click(screen.getByText("Remove test track"));
    expect(screen.queryByText("Track 1")).toBeNull();
    await act(async () => {
      fireEvent.click(screen.getByText("Track 2"));
    });
    expect(mocks.playFromSource).toHaveBeenLastCalledWith(second, [second], {
      source: expect.objectContaining({ allTracks: [second] }),
    });
    view.unmount();
    scroller.remove();
  });

  it("reuses library cards while a page is pending, then updates appended data, favorites, and sorting", async () => {
    const page = deferred<{
      items: ReturnType<typeof album>[];
      totalNumberOfItems: number;
    }>();
    const first = album(1);
    const second = album(2);
    mocks.albums.mockResolvedValueOnce({
      items: [first],
      totalNumberOfItems: 2,
    });
    mocks.albums.mockReturnValueOnce(page.promise);
    const store = makeStore();
    render(
      <Provider store={store}>
        <LibraryViewAll libraryType="albums" />
      </Provider>,
    );
    await screen.findByText("Album 1");
    mocks.cards.mockClear();

    act(() => {
      void getRestoreLoader()?.loadMore();
    });
    expect(mocks.albums).toHaveBeenCalledTimes(2);
    expect(mocks.cards).not.toHaveBeenCalled();
    await act(async () => {
      page.resolve({ items: [second], totalNumberOfItems: 2 });
    });
    expect(screen.getByText("Album 2")).toBeTruthy();
    expect(mocks.cards).toHaveBeenCalledTimes(2);

    mocks.cards.mockClear();
    act(() => {
      store.set(favoriteAlbumIdsAtom, new Set([1]));
    });
    expect(
      mocks.cards.mock.calls.find(([props]) => props.item.id === 1)?.[0]
        .isFavorited,
    ).toBe(true);
    fireEvent.click(screen.getByText("Album 2"));
    expect(mocks.navigateToAlbum).toHaveBeenCalledWith(
      2,
      expect.objectContaining({ title: "Album 2" }),
    );

    mocks.albums.mockResolvedValueOnce({
      items: [second, first],
      totalNumberOfItems: 2,
    });
    fireEvent.click(screen.getByTitle(/Sort by:/));
    fireEvent.click(screen.getByText("Name"));
    await screen.findByText("Album 1");
    expect(mocks.albums).toHaveBeenLastCalledWith(7, 0, 50, "NAME", "ASC");
  });

  it("keeps folder title, count, navigation, and context menu current", async () => {
    mocks.folders.mockResolvedValue({
      items: [
        {
          itemType: "FOLDER",
          name: "Folder",
          trn: "trn:folder:folder",
          data: { totalNumberOfItems: 2 },
        },
      ],
      totalNumberOfItems: 1,
      cursor: null,
    });
    const store = makeStore();
    render(
      <Provider store={store}>
        <LibraryViewAll libraryType="playlists" />
      </Provider>,
    );
    await screen.findByText("Folder");
    act(() => {
      store.set(renamedFoldersAtom, new Map([["folder", "Renamed folder"]]));
      store.set(folderCountAdjustmentsAtom, new Map([["folder", 3]]));
    });
    expect(screen.getByText("5 playlists")).toBeTruthy();
    fireEvent.click(screen.getByText("Renamed folder"));
    expect(mocks.navigateToPlaylistFolder).toHaveBeenCalledWith(
      "folder",
      "Renamed folder",
    );
    fireEvent.contextMenu(screen.getByText("Renamed folder"));
    expect(screen.getByRole("dialog").textContent).toBe("Renamed folder");
  });
});
