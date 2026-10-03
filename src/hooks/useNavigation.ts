import { startTransition } from "react";
import { useStore } from "jotai";
import { currentViewAtom } from "../atoms/navigation";
import { drawerOpenAtom, maximizedPlayerAtom } from "../atoms/ui";
import { pushView } from "../lib/scrollMemory";
import type { AppView, ProfilePlaylist } from "../types";

function createNavigationActions(store: ReturnType<typeof useStore>) {
  // NOTE: Popstate listener lives in AppInitializer (closes overlays there too).

  // Every navigation dismisses the player overlays (Queue View + fullscreen
  // player) so the destination page isn't left hidden behind them.
  const navigate = (view: AppView) => {
    store.set(drawerOpenAtom, false);
    store.set(maximizedPlayerAtom, false);
    const stamped = pushView(view);
    // Wrap in startTransition so React can show the new page's skeleton
    // immediately without blocking on unmounting the old page's heavy DOM.
    startTransition(() => {
      store.set(currentViewAtom, stamped);
    });
  };

  const navigateToAlbum = (
    albumId: number,
    albumInfo?: { title: string; cover?: string; artistName?: string },
  ) => {
    navigate({ type: "album", albumId, albumInfo });
  };

  const navigateToPlaylist = (
    playlistId: string,
    playlistInfo?: {
      title: string;
      image?: string;
      description?: string;
      creatorName?: string;
      numberOfTracks?: number;
      numberOfVideos?: number;
      isUserPlaylist?: boolean;
    },
  ) => {
    navigate({ type: "playlist", playlistId, playlistInfo });
  };

  const navigateToFavorites = () => {
    navigate({ type: "favorites" });
  };

  const navigateHome = () => {
    navigate({ type: "home" });
  };

  const navigateToSearch = (query: string) => {
    navigate({ type: "search", query });
  };

  const navigateToViewAll = (
    title: string,
    apiPath: string,
    artistId?: number,
  ) => {
    navigate({ type: "viewAll", title, apiPath, artistId });
  };

  const navigateToArtist = (
    artistId: number,
    artistInfo?: { name: string; picture?: string },
  ) => {
    navigate({ type: "artist", artistId, artistInfo });
  };

  const navigateToMix = (
    mixId: string,
    mixInfo?: {
      title: string;
      image?: string;
      subtitle?: string;
      mixType?: string;
      artistId?: number;
      artistName?: string;
      artistPicture?: string;
    },
  ) => {
    navigate({ type: "mix", mixId, mixInfo });
  };

  const navigateToArtistTracks = (artistId: number, artistName: string) => {
    navigate({ type: "artistTracks", artistId, artistName });
  };

  const navigateToProfile = () => {
    navigate({ type: "profile" });
  };

  const navigateToProfilePlaylists = (
    playlists: ProfilePlaylist[],
    profileName: string,
  ) => {
    navigate({ type: "profilePlaylists", playlists, profileName });
  };

  const navigateToExplore = () => {
    navigate({ type: "explore" });
  };

  const navigateToExplorePage = (apiPath: string, title: string) => {
    navigate({ type: "explorePage", apiPath, title });
  };

  const navigateToFeed = () => {
    navigate({ type: "feed" });
  };

  const navigateToLibraryViewAll = (
    libraryType: "playlists" | "albums" | "artists" | "mixes",
  ) => {
    navigate({ type: "libraryViewAll", libraryType });
  };

  const navigateToPlaylistFolder = (folderId: string, folderName: string) => {
    navigate({
      type: "libraryViewAll",
      libraryType: "playlists",
      folderId,
      folderName,
    });
  };

  return {
    navigateToAlbum,
    navigateToPlaylist,
    navigateToFavorites,
    navigateHome,
    navigateToSearch,
    navigateToViewAll,
    navigateToArtist,
    navigateToArtistTracks,
    navigateToMix,
    navigateToProfile,
    navigateToProfilePlaylists,
    navigateToExplore,
    navigateToExplorePage,
    navigateToFeed,
    navigateToLibraryViewAll,
    navigateToPlaylistFolder,
  };
}

// Virtual rows mount repeatedly while scrolling. These actions only depend on
// the store, so share them across consumers instead of recreating every callback
// for each row. Weak keys preserve isolation without retaining disposed stores.
const navigationActions = new WeakMap<
  ReturnType<typeof useStore>,
  ReturnType<typeof createNavigationActions>
>();

export function useNavigation() {
  const store = useStore();
  let actions = navigationActions.get(store);
  if (!actions) {
    actions = createNavigationActions(store);
    navigationActions.set(store, actions);
  }
  return actions;
}
