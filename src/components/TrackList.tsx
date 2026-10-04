import {
  Play,
  Heart,
  MoreHorizontal,
  Plus,
  ListPlus,
  ChevronUp,
  ChevronDown,
  Video,
} from "lucide-react";
import { type Track, getTidalImageUrl, getTrackDisplayTitle } from "../types";
import ExplicitBadge from "./ExplicitBadge";
import TidalImage from "./TidalImage";
import AddToPlaylistMenu from "./AddToPlaylistMenu";
import TrackContextMenu from "./TrackContextMenu";
import {
  useRef,
  useEffect,
  useLayoutEffect,
  useState,
  memo,
  useMemo,
  useCallback,
  createContext,
  useContext,
  type Key,
} from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { useAtomValue, atom } from "jotai";
import {
  currentTrackAtom,
  isPlayingAtom,
  allowExplicitAtom,
} from "../atoms/playback";
import { favoriteTrackIdsAtom, favoriteVideoIdsAtom } from "../atoms/favorites";
import { useNavigation } from "../hooks/useNavigation";
import { useFavoriteActions } from "../hooks/useFavorites";
import { useToast } from "../contexts/ToastContext";
import { usePageScrollElement } from "../contexts/PageScrollContext";
import { isTrackUnavailable } from "../lib/trackAvailability";
import { TrackArtists } from "./TrackArtists";
import { formatDateAdded } from "../lib/formatDateAdded";

interface TrackListProps {
  tracks: Track[];
  onPlay: (track: Track, index: number) => void;
  showDateAdded?: boolean;
  showAlbum?: boolean;
  showCover?: boolean;
  showArtist?: boolean;
  onLoadMore?: () => void;
  hasMore?: boolean;
  loadingMore?: boolean;
  context?: "album" | "playlist" | "favorites" | "search";
  /** Optional per-row display numbers (e.g. original playlist position when filtering) */
  trackDisplayNumbers?: number[];
  /** For "Remove from playlist" support */
  playlistId?: string;
  isUserPlaylist?: boolean;
  onTrackRemoved?: (index: number) => void;
  /** When provided, shows a dedicated "add to this playlist" button (immediate action, no menu) */
  onAddToCurrentPlaylist?: (track: Track) => void;
  sortable?: boolean;
  sortColumn?: string | null;
  sortDirection?: "ASC" | "DESC" | null;
  onSort?: (column: string | null, direction: "ASC" | "DESC" | null) => void;
  sortLoading?: boolean;
  /** When true, rows render through @tanstack/react-virtual. Only paginated
   * callers (Loved tracks, Playlists) use this. */
  virtualize?: boolean;
}

function formatDuration(seconds: number): string {
  const mins = Math.floor(seconds / 60);
  const secs = seconds % 60;
  return `${mins}:${secs.toString().padStart(2, "0")}`;
}

type TrackRowActions = Pick<
  ReturnType<typeof useFavoriteActions>,
  | "addFavoriteTrack"
  | "removeFavoriteTrack"
  | "addFavoriteVideo"
  | "removeFavoriteVideo"
> &
  Pick<ReturnType<typeof useNavigation>, "navigateToAlbum">;

const TrackRowActionsContext = createContext<TrackRowActions | null>(null);

// ─── Memoized TrackRow ─────────────────────────────────────────────────────

interface TrackRowProps {
  track: Track;
  index: number;
  displayNumber?: number;
  gridCols: string;
  showCover: boolean;
  showArtist: boolean;
  showAlbum: boolean;
  showDateAdded: boolean;
  context: string;
  onPlay: (track: Track, index: number) => void;
  playlistId?: string;
  isUserPlaylist?: boolean;
  onTrackRemoved?: (index: number) => void;
  onAddToCurrentPlaylist?: (track: Track) => void;
  /** Fast scrolling turns hover transitions off on rows that stay mounted. */
  quiet?: boolean;
}

const TrackRow = memo(function TrackRow({
  track,
  index,
  displayNumber,
  gridCols,
  showCover,
  showArtist,
  showAlbum,
  showDateAdded,
  context,
  onPlay,
  playlistId,
  isUserPlaylist,
  onTrackRemoved,
  onAddToCurrentPlaylist,
  quiet = false,
}: TrackRowProps) {
  const rowActions = useContext(TrackRowActionsContext);
  if (!rowActions) throw new Error("TrackRow requires TrackList actions");
  const {
    navigateToAlbum,
    addFavoriteTrack,
    removeFavoriteTrack,
    addFavoriteVideo,
    removeFavoriteVideo,
  } = rowActions;
  const { showToast } = useToast();

  const isVideo = track.itemType === "video";
  const isFavoriteAtom = useMemo(
    () =>
      atom((get) =>
        get(isVideo ? favoriteVideoIdsAtom : favoriteTrackIdsAtom).has(
          track.id,
        ),
      ),
    [track.id, isVideo],
  );
  const isFav = useAtomValue(isFavoriteAtom);

  const [playlistMenuOpen, setPlaylistMenuOpen] = useState(false);
  const [contextMenuOpen, setContextMenuOpen] = useState(false);
  const [contextMenuCursorPos, setContextMenuCursorPos] = useState<
    { x: number; y: number } | undefined
  >(undefined);
  const plusButtonRef = useRef<HTMLButtonElement>(null);
  const dotsButtonRef = useRef<HTMLButtonElement>(null);

  const allowExplicit = useAtomValue(allowExplicitAtom);
  const isBlocked = !allowExplicit && !!track.explicit;

  const isActiveAtom = useMemo(
    () => atom((get) => (get(currentTrackAtom)?.id ?? null) === track.id),
    [track.id],
  );
  const isActive = useAtomValue(isActiveAtom);

  const isPlayingHereAtom = useMemo(
    () =>
      atom(
        (get) =>
          (get(currentTrackAtom)?.id ?? null) === track.id &&
          get(isPlayingAtom),
      ),
    [track.id],
  );
  const playing = useAtomValue(isPlayingHereAtom);

  // Don't grey out the actively-playing row even if its metadata has been
  // refreshed to streamReady:false — audio is the source of truth. Videos are
  // never gated on audio stream flags.
  const isUnavailable = !isVideo && isTrackUnavailable(track) && !isActive;
  const isInactive = isBlocked || isUnavailable;

  const toggleFavorite = async (e: React.MouseEvent) => {
    e.stopPropagation();
    try {
      if (isVideo) {
        if (isFav) {
          await removeFavoriteVideo(track.id);
        } else {
          await addFavoriteVideo(track.id);
        }
      } else if (isFav) {
        await removeFavoriteTrack(track.id);
      } else {
        await addFavoriteTrack(track.id, track);
      }
    } catch (err) {
      console.error("Failed to toggle favorite", err);
    }
  };

  const handlePlusClick = (e: React.MouseEvent) => {
    e.stopPropagation();
    setContextMenuOpen(false);
    setPlaylistMenuOpen((prev) => !prev);
  };

  const handleDotsClick = (e: React.MouseEvent) => {
    e.stopPropagation();
    setPlaylistMenuOpen(false);
    setContextMenuCursorPos(undefined);
    setContextMenuOpen((prev) => !prev);
  };

  const handleRowContextMenu = (e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    setPlaylistMenuOpen(false);
    setContextMenuCursorPos({ x: e.clientX, y: e.clientY });
    setContextMenuOpen(true);
  };

  return (
    <div
      onClick={() => {
        if (isBlocked) return;
        if (isUnavailable) {
          showToast("Track unavailable", "info");
          return;
        }
        onPlay(track, index);
      }}
      onContextMenu={isBlocked ? undefined : handleRowContextMenu}
      data-track-row="full"
      className={`grid gap-4 px-4 py-2.5 rounded-md items-center ${
        isInactive
          ? "opacity-40 cursor-default"
          : `cursor-pointer ${isActive ? "bg-th-hl-faint" : ""} ${
              /* Hover curves stay off while the list is moving. A transition
                 on every visible row is extra work on the fullscreen path. */
              quiet
                ? ""
                : "group transition-[background-color,color] duration-150 ease-settle hover:bg-th-hl-faint active:bg-th-hl-med motion-reduce:transition-none"
            }`
      }`}
      style={{ gridTemplateColumns: gridCols }}
    >
      {/* Track Number / Playing Indicator */}
      <div className="relative flex items-center justify-end">
        {playing ? (
          <div className="flex items-end gap-[3px] h-4">
            <span className="w-[3px] h-full bg-th-accent rounded-full playing-bar" />
            <span
              className="w-[3px] h-full bg-th-accent rounded-full playing-bar"
              style={{ animationDelay: "0.2s" }}
            />
            <span
              className="w-[3px] h-full bg-th-accent rounded-full playing-bar"
              style={{ animationDelay: "0.4s" }}
            />
          </div>
        ) : (
          <>
            <span
              className={`text-[15px] tabular-nums ${
                quiet
                  ? ""
                  : "transition-opacity duration-150 ease-settle motion-reduce:transition-none group-hover:opacity-0"
              } ${isActive ? "text-th-accent" : "text-th-text-muted"}`}
            >
              {displayNumber != null
                ? displayNumber
                : context === "album"
                  ? (track.trackNumber ?? index + 1)
                  : index + 1}
            </span>
            {!quiet && (
              <Play
                size={14}
                fill="currentColor"
                className="absolute right-0 text-th-text-primary opacity-0 group-hover:opacity-100 transition-opacity duration-150 ease-settle motion-reduce:transition-none"
              />
            )}
          </>
        )}
      </div>

      {/* Title + Thumbnail */}
      <div className="flex items-center gap-3 min-w-0">
        {showCover && (
          <div className="relative w-10 h-10 shrink-0 rounded bg-th-surface-hover overflow-hidden">
            <TidalImage
              src={getTidalImageUrl(
                isVideo ? track.imageId : track.album?.cover,
                160,
              )}
              alt={track.album?.title || track.title}
              className="w-full h-full object-cover"
            />
            {isVideo && (
              <div className="absolute bottom-0.5 right-0.5 flex items-center justify-center w-4 h-4 rounded bg-black/70">
                <Video size={10} className="text-white" />
              </div>
            )}
          </div>
        )}
        <div className="flex flex-col justify-center min-w-0">
          <div className="flex items-center gap-1.5 min-w-0">
            <span
              className={`text-[15px] font-medium truncate leading-snug ${
                isActive ? "text-th-accent" : "text-th-text-primary"
              }`}
            >
              {getTrackDisplayTitle(track)}
            </span>
            {isVideo && (
              <span className="shrink-0 inline-flex items-center justify-center px-1 h-[15px] rounded-[3px] bg-th-text-faint/15 text-th-text-muted text-[9px] font-bold leading-none tracking-wide">
                VIDEO
              </span>
            )}
            {track.explicit && <ExplicitBadge />}
          </div>
          {!showArtist && (
            <span className="text-[13px] text-th-text-muted truncate leading-snug">
              <TrackArtists
                artists={track.artists}
                artist={track.artist}
                className="hover:text-th-text-primary hover:underline transition-colors cursor-pointer"
              />
            </span>
          )}
        </div>
      </div>

      {/* Artist (Column) */}
      {showArtist && (
        <div className="flex items-center min-w-0">
          <span className="text-[14px] text-th-text-muted truncate">
            <TrackArtists
              artists={track.artists}
              artist={track.artist}
              className="hover:text-th-text-primary hover:underline transition-colors cursor-pointer"
            />
          </span>
        </div>
      )}

      {/* Album */}
      {showAlbum && (
        <div className="flex items-center min-w-0">
          <span
            className="text-[14px] text-th-text-muted truncate hover:text-th-text-primary hover:underline transition-colors cursor-pointer"
            onClick={(e) => {
              e.stopPropagation();
              if (track.album?.id) {
                navigateToAlbum(track.album.id, {
                  title: track.album.title,
                  cover: track.album.cover,
                  artistName: track.artist?.name,
                });
              }
            }}
          >
            {track.album?.title || ""}
          </span>
        </div>
      )}

      {/* Date Added */}
      {showDateAdded && (
        <div className="flex items-center min-w-0">
          <span className="text-[14px] text-th-text-muted truncate">
            {formatDateAdded(track.dateAdded)}
          </span>
        </div>
      )}

      {/* Duration */}
      <div className="flex items-center justify-end text-[14px] text-th-text-muted tabular-nums">
        {formatDuration(track.duration)}
      </div>

      {/* Actions */}
      <div className="flex items-center justify-end gap-2">
        <button
          ref={dotsButtonRef}
          className={`p-1.5 rounded-full transition-colors ${
            isBlocked
              ? "hidden"
              : contextMenuOpen
                ? "text-th-text-primary opacity-100"
                : quiet
                  ? "text-th-text-muted opacity-0"
                  : "text-th-text-muted hover:text-th-text-primary opacity-0 group-hover:opacity-100"
          }`}
          title="More options"
          onClick={handleDotsClick}
          disabled={isBlocked}
        >
          <MoreHorizontal size={18} />
        </button>
        {contextMenuOpen && (
          <TrackContextMenu
            track={track}
            index={index}
            anchorRef={dotsButtonRef}
            cursorPosition={contextMenuCursorPos}
            onClose={() => setContextMenuOpen(false)}
            playlistId={playlistId}
            isUserPlaylist={isUserPlaylist}
            onTrackRemoved={onTrackRemoved}
          />
        )}
        {onAddToCurrentPlaylist ? (
          <button
            className="p-1.5 rounded-full transition-colors text-th-text-muted hover:text-th-accent"
            title="Add to this playlist"
            onClick={(e) => {
              e.stopPropagation();
              onAddToCurrentPlaylist(track);
            }}
          >
            <ListPlus size={18} />
          </button>
        ) : (
          <>
            <button
              ref={plusButtonRef}
              className={`p-1.5 rounded-full transition-colors ${
                playlistMenuOpen
                  ? "text-th-accent"
                  : "text-th-text-muted hover:text-th-text-primary"
              }`}
              title="Add to playlist"
              onClick={handlePlusClick}
            >
              <Plus size={18} />
            </button>
            {playlistMenuOpen && (
              <AddToPlaylistMenu
                trackIds={[track.id]}
                anchorRef={plusButtonRef}
                onClose={() => setPlaylistMenuOpen(false)}
              />
            )}
          </>
        )}
        <button
          className={`p-1.5 rounded-full transition-colors ${isFav ? "text-th-accent" : "text-th-text-muted hover:text-th-text-primary"}`}
          title={isFav ? "Remove from favorites" : "Add to favorites"}
          onClick={toggleFavorite}
        >
          <Heart size={18} fill={isFav ? "currentColor" : "none"} />
        </button>
      </div>
    </div>
  );
});

function artistLabel(track: Track): string {
  if (track.artists && track.artists.length > 0) {
    return track.artists.map((artist) => artist.name).join(", ");
  }
  return track.artist?.name || "Unknown Artist";
}

// Text-only stand-in for a row that enters during a fast flick. Same grid and
// row height as TrackRow, without images, favorite subscriptions, or menus.
const TrackRowShell = memo(function TrackRowShell({
  track,
  index,
  displayNumber,
  gridCols,
  showCover,
  showArtist,
  showAlbum,
  showDateAdded,
  context,
  onPlay,
  allowExplicit,
  currentTrackId,
}: TrackRowProps & {
  allowExplicit: boolean;
  currentTrackId: number | null;
}) {
  const number =
    displayNumber != null
      ? displayNumber
      : context === "album"
        ? (track.trackNumber ?? index + 1)
        : index + 1;
  const blocked = !allowExplicit && !!track.explicit;
  const unavailable =
    track.itemType !== "video" &&
    isTrackUnavailable(track) &&
    currentTrackId !== track.id;
  const inactive = blocked || unavailable;
  const artists = artistLabel(track);

  return (
    <div
      data-track-row="shell"
      onClick={() => {
        if (inactive) return;
        onPlay(track, index);
      }}
      className={`grid gap-4 px-4 py-2.5 rounded-md items-center ${
        inactive
          ? "opacity-40 cursor-default"
          : "cursor-pointer active:bg-th-hl-faint"
      }`}
      style={{ gridTemplateColumns: gridCols }}
    >
      <div className="flex items-center justify-end">
        <span className="text-[15px] tabular-nums text-th-text-muted">
          {number}
        </span>
      </div>
      <div className="flex items-center gap-3 min-w-0">
        {showCover && (
          <div className="w-10 h-10 shrink-0 rounded bg-th-surface-hover" />
        )}
        <div className="flex flex-col justify-center min-w-0">
          <div className="flex items-center gap-1.5 min-w-0">
            <span className="text-[15px] font-medium truncate leading-snug text-th-text-primary">
              {getTrackDisplayTitle(track)}
            </span>
            {track.itemType === "video" && (
              <span className="shrink-0 inline-flex items-center justify-center px-1 h-[15px] rounded-[3px] bg-th-text-faint/15 text-th-text-muted text-[9px] font-bold leading-none tracking-wide">
                VIDEO
              </span>
            )}
            {track.explicit && <ExplicitBadge />}
          </div>
          {!showArtist && (
            <span className="text-[13px] text-th-text-muted truncate leading-snug">
              {artists}
            </span>
          )}
        </div>
      </div>
      {showArtist && (
        <div className="flex items-center min-w-0">
          <span className="text-[14px] text-th-text-muted truncate">
            {artists}
          </span>
        </div>
      )}
      {showAlbum && (
        <div className="flex items-center min-w-0">
          <span className="text-[14px] text-th-text-muted truncate">
            {track.album?.title || ""}
          </span>
        </div>
      )}
      {showDateAdded && (
        <div className="flex items-center min-w-0">
          <span className="text-[14px] text-th-text-muted truncate">
            {formatDateAdded(track.dateAdded)}
          </span>
        </div>
      )}
      <div className="flex items-center justify-end text-[14px] text-th-text-muted tabular-nums">
        {formatDuration(track.duration)}
      </div>
      <div />
    </div>
  );
});

// ─── VirtualTrackRows ──────────────────────────────────────────────────────

interface VirtualTrackRowsProps {
  tracks: Track[];
  gridCols: string;
  showCover: boolean;
  showArtist: boolean;
  showAlbum: boolean;
  showDateAdded: boolean;
  context: string;
  onPlay: (track: Track, index: number) => void;
  trackDisplayNumbers?: number[];
  playlistId?: string;
  isUserPlaylist?: boolean;
  onTrackRemoved?: (index: number) => void;
  onAddToCurrentPlaylist?: (track: Track) => void;
  onLoadMore?: () => void;
  hasMore?: boolean;
  loadingMore?: boolean;
}

function VirtualTrackRows({
  tracks,
  gridCols,
  showCover,
  showArtist,
  showAlbum,
  showDateAdded,
  context,
  onPlay,
  trackDisplayNumbers,
  playlistId,
  isUserPlaylist,
  onTrackRemoved,
  onAddToCurrentPlaylist,
  onLoadMore,
  hasMore,
  loadingMore,
}: VirtualTrackRowsProps) {
  const parentRef = useRef<HTMLDivElement>(null);
  const scrollEl = usePageScrollElement();
  const [scrollMargin, setScrollMargin] = useState(0);
  const allowExplicit = useAtomValue(allowExplicitAtom);
  const currentTrackId = useAtomValue(currentTrackAtom)?.id ?? null;
  // Keys mounted as full rows before this gesture. The virtualizer re-renders
  // only when the index range changes, and each entering playlist row is one
  // mount. Home's feed does not virtualize, so it pays nothing here.
  const idleKeysRef = useRef<Set<Key>>(new Set());
  const heldFullKeysRef = useRef<Set<Key> | null>(null);

  // Maintain scrollMargin: the list's offset within the scroll container.
  // Summed from the offsetParent chain rather than from rects plus scrollTop:
  // the rows this value positions, and the overflow they create, feed back into
  // the parent's rect, so a rect-derived margin diverges as the user scrolls
  // (736 -> 503 -> -1311 -> -3689 was the observed decay, which translated every
  // row thousands of pixels off-screen). offsetTop excludes scroll entirely.
  // Relies on Layout's container being a positioned ancestor, which it is.
  useLayoutEffect(() => {
    if (!parentRef.current || !scrollEl) return;
    const measure = () => {
      let node: HTMLElement | null = parentRef.current;
      let top = 0;
      for (let hops = 0; node && node !== scrollEl && hops < 32; hops++) {
        top += node.offsetTop;
        node = node.offsetParent as HTMLElement | null;
      }
      setScrollMargin(top);
    };
    measure();

    const ro = new ResizeObserver(measure);
    ro.observe(parentRef.current);
    if (scrollEl.firstElementChild) ro.observe(scrollEl.firstElementChild);
    ro.observe(scrollEl);

    return () => ro.disconnect();
  }, [scrollEl]);

  // Load-bearing for the page's scrollHeight: 60 = a 40px cover + 20px py-2.5.
  // If this drifts from a real row's height the scrollbar shifts under a restore.
  // The 48 branch is unexercised — no caller passes no-cover — and is not
  // derived; a real no-cover row is ~50px with showArtist, ~58px without.
  const rowHeight = showCover ? 60 : 48;
  // Virtual-core invalidates every row measurement when this callback changes.
  // Scrolling must reuse it; replacing/reordering tracks must invalidate it.
  const getItemKey = useCallback(
    (index: number) => tracks[index]?.id ?? index,
    [tracks],
  );

  const virtualizer = useVirtualizer({
    count: tracks.length,
    getScrollElement: () => scrollEl,
    estimateSize: () => rowHeight,
    overscan: 8,
    // Covers return this long after the last scroll event.
    isScrollingResetDelay: 120,
    scrollMargin,
    // The virtualizer scrolls its element to initialOffset once on attach, and
    // the default 0 would wipe a restored offset the moment this list mounts.
    // Resolved once and memoised on first read, so this relies on Layout's
    // element already being attached before any virtualized list first renders.
    initialOffset: () => scrollEl?.scrollTop ?? 0,
    getItemKey,
  });

  const virtualItems = virtualizer.getVirtualItems();
  const scrolling = virtualizer.isScrolling;
  if (!scrolling) {
    heldFullKeysRef.current = null;
  } else if (heldFullKeysRef.current === null) {
    heldFullKeysRef.current = idleKeysRef.current;
  }
  const heldFullKeys = heldFullKeysRef.current;
  if (scrolling && heldFullKeys) {
    const visible = new Set(virtualItems.map((item) => item.key));
    for (const key of heldFullKeys) {
      if (!visible.has(key)) heldFullKeys.delete(key);
    }
  } else {
    idleKeysRef.current = new Set(virtualItems.map((item) => item.key));
  }

  // Sentinel-based load trigger: an IntersectionObserver watches a sentinel
  // positioned near the end of the virtualized spacer. Mirrors the
  // non-virtualized path's pagination semantics, and avoids the race where
  // overscan rows alone would satisfy a count-based threshold on mount.
  const sentinelRef = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    if (!onLoadMore) return;
    const sentinel = sentinelRef.current;
    if (!sentinel) return;
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries[0].isIntersecting && hasMore) {
          onLoadMore();
        }
      },
      { threshold: 0.1 },
    );
    observer.observe(sentinel);
    return () => observer.disconnect();
    // tracks.length re-arms the observer on growth: a restored offset can park
    // the viewport at max scroll with the sentinel already inside it, and
    // IntersectionObserver reports only crossings, never a standing overlap.
  }, [hasMore, onLoadMore, tracks.length]);

  return (
    <>
      <div
        ref={parentRef}
        style={{
          height: virtualizer.getTotalSize(),
          position: "relative",
          width: "100%",
        }}
      >
        {virtualItems.map((v) => {
          const track = tracks[v.index];
          if (!track) return null;
          // Rows already on screen stay full. Rows that enter while the list
          // is moving are text, including ones that scrolled away and came back.
          const shell = scrolling && !heldFullKeys?.has(v.key);
          const rowProps = {
            track,
            index: v.index,
            displayNumber: trackDisplayNumbers?.[v.index],
            gridCols,
            showCover,
            showArtist,
            showAlbum,
            showDateAdded,
            context,
            onPlay,
          };
          return (
            // Deliberately unmeasured: estimateSize is exact for a fixed-height
            // row, and a hidden list would measure every row as 0 and make the
            // virtualizer scroll-adjust the shared container.
            <div
              key={v.key}
              data-index={v.index}
              style={{
                position: "absolute",
                // translateY would promote every row to its own full-width
                // composited layer. That stack's pixel cost follows the window,
                // so a fullscreen playlist misses frames on WebKitGTK while a
                // half or quarter window still scrolls. Document `top` stays
                // inside the single scroll layer.
                top: v.start - virtualizer.options.scrollMargin,
                left: 0,
                right: 0,
                height: rowHeight,
              }}
            >
              {shell ? (
                <TrackRowShell
                  {...rowProps}
                  allowExplicit={allowExplicit}
                  currentTrackId={currentTrackId}
                />
              ) : (
                <TrackRow
                  {...rowProps}
                  quiet={scrolling}
                  playlistId={playlistId}
                  isUserPlaylist={isUserPlaylist}
                  onTrackRemoved={onTrackRemoved}
                  onAddToCurrentPlaylist={onAddToCurrentPlaylist}
                />
              )}
            </div>
          );
        })}
        {hasMore && (
          <div
            ref={sentinelRef}
            aria-hidden
            style={{
              position: "absolute",
              top: Math.max(0, virtualizer.getTotalSize() - rowHeight * 10),
              left: 0,
              right: 0,
              height: 1,
              pointerEvents: "none",
            }}
          />
        )}
      </div>

      {/* Pagination skeletons — kept identical to the non-virtualized path. */}
      {hasMore && loadingMore && (
        <div className="flex flex-col">
          {Array.from({ length: 5 }).map((_, i) => (
            <div
              key={i}
              className="grid gap-4 px-4 py-2.5"
              style={{ gridTemplateColumns: gridCols }}
            >
              <div className="flex items-center justify-end">
                <div className="h-4 w-5 bg-th-surface-hover rounded" />
              </div>
              <div className="flex items-center gap-3 min-w-0">
                {showCover && (
                  <div className="w-10 h-10 shrink-0 rounded bg-th-surface-hover" />
                )}
                <div className="flex flex-col gap-1.5 min-w-0 flex-1">
                  <div className="h-4 w-3/5 bg-th-surface-hover rounded" />
                  <div className="h-3 w-2/5 bg-th-surface-hover/60 rounded" />
                </div>
              </div>
              {showArtist && (
                <div className="flex items-center">
                  <div className="h-3.5 w-3/5 bg-th-surface-hover/60 rounded" />
                </div>
              )}
              {showAlbum && (
                <div className="flex items-center">
                  <div className="h-3.5 w-3/5 bg-th-surface-hover/60 rounded" />
                </div>
              )}
              {showDateAdded && (
                <div className="flex items-center">
                  <div className="h-3.5 w-2/5 bg-th-surface-hover/60 rounded" />
                </div>
              )}
              <div className="flex items-center justify-end">
                <div className="h-3.5 w-8 bg-th-surface-hover/60 rounded" />
              </div>
              <div />
            </div>
          ))}
        </div>
      )}
    </>
  );
}

// ─── SortIndicator ─────────────────────────────────────────────────────────

function SortIndicator({ direction }: { direction: "ASC" | "DESC" }) {
  return direction === "ASC" ? (
    <ChevronUp size={14} className="inline ml-0.5" />
  ) : (
    <ChevronDown size={14} className="inline ml-0.5" />
  );
}

// ─── TrackList ─────────────────────────────────────────────────────────────

const TrackListContent = memo(function TrackListContent({
  tracks,
  onPlay,
  showDateAdded = false,
  showAlbum = true,
  showCover = true,
  showArtist = true,
  onLoadMore,
  hasMore = false,
  loadingMore = false,
  context = "playlist",
  trackDisplayNumbers,
  playlistId,
  isUserPlaylist,
  onTrackRemoved,
  onAddToCurrentPlaylist,
  sortable = false,
  sortColumn,
  sortDirection,
  onSort,
  sortLoading = false,
  virtualize = false,
}: TrackListProps) {
  const sentinelRef = useRef<HTMLDivElement | null>(null);
  const observerRef = useRef<IntersectionObserver | null>(null);

  useEffect(() => {
    if (virtualize) return;
    if (!onLoadMore) return;

    if (observerRef.current) {
      observerRef.current.disconnect();
    }

    observerRef.current = new IntersectionObserver(
      (entries) => {
        if (entries[0].isIntersecting && hasMore) {
          onLoadMore();
        }
      },
      { threshold: 0.1 },
    );

    if (sentinelRef.current) {
      observerRef.current.observe(sentinelRef.current);
    }

    return () => observerRef.current?.disconnect();
  }, [hasMore, onLoadMore, virtualize, tracks.length]);

  // Build grid columns string
  const gridCols = useMemo(
    () =>
      [
        "36px", // #
        showCover ? "minmax(200px, 4fr)" : "minmax(200px, 4fr)", // Title (with or without cover)
        ...(showArtist ? ["minmax(120px, 2fr)"] : []),
        ...(showAlbum ? ["minmax(120px, 2fr)"] : []),
        ...(showDateAdded ? ["minmax(100px, 1fr)"] : []),
        "72px", // Time
        "100px", // Actions (always present for + and heart)
      ].join(" "),
    [showCover, showArtist, showAlbum, showDateAdded],
  );

  const handleHeaderClick = (column: string) => {
    if (!onSort) return;
    if (sortColumn === column) {
      // Toggle direction
      onSort(column, sortDirection === "ASC" ? "DESC" : "ASC");
    } else {
      onSort(column, "ASC");
    }
  };

  return (
    <div className="flex flex-col w-full">
      {/* Header Row */}
      <div
        className="grid gap-4 px-4 py-3 border-b border-th-inset text-[12px] text-th-text-muted uppercase tracking-widest mb-2"
        style={{ gridTemplateColumns: gridCols }}
      >
        <span className="text-right">#</span>
        <span>
          {sortable ? (
            <span
              className="cursor-pointer select-none hover:text-th-accent whitespace-nowrap"
              onClick={() => handleHeaderClick("NAME")}
            >
              Title
              {sortColumn === "NAME" && sortDirection && (
                <SortIndicator direction={sortDirection} />
              )}
            </span>
          ) : (
            "Title"
          )}
        </span>
        {showArtist && (
          <span>
            {sortable ? (
              <span
                className="cursor-pointer select-none hover:text-th-accent whitespace-nowrap"
                onClick={() => handleHeaderClick("ARTIST")}
              >
                Artist
                {sortColumn === "ARTIST" && sortDirection && (
                  <SortIndicator direction={sortDirection} />
                )}
              </span>
            ) : (
              "Artist"
            )}
          </span>
        )}
        {showAlbum && (
          <span>
            {sortable ? (
              <span
                className="cursor-pointer select-none hover:text-th-accent whitespace-nowrap"
                onClick={() => handleHeaderClick("ALBUM")}
              >
                Album
                {sortColumn === "ALBUM" && sortDirection && (
                  <SortIndicator direction={sortDirection} />
                )}
              </span>
            ) : (
              "Album"
            )}
          </span>
        )}
        {showDateAdded && (
          <span>
            {sortable ? (
              <span
                className="cursor-pointer select-none hover:text-th-accent whitespace-nowrap"
                onClick={() => handleHeaderClick("DATE")}
              >
                Date Added
                {sortColumn === "DATE" && sortDirection && (
                  <SortIndicator direction={sortDirection} />
                )}
              </span>
            ) : (
              "Date Added"
            )}
          </span>
        )}
        <span className="text-right">
          {sortable ? (
            <span
              className="cursor-pointer select-none hover:text-th-accent whitespace-nowrap"
              onClick={() => handleHeaderClick("LENGTH")}
            >
              Time
              {sortColumn === "LENGTH" && sortDirection && (
                <SortIndicator direction={sortDirection} />
              )}
            </span>
          ) : (
            "Time"
          )}
        </span>
        <span /> {/* Actions column header */}
      </div>

      {/* Track Rows */}
      <div className="flex flex-col">
        {sortLoading ? (
          Array.from({ length: 20 }).map((_, i) => (
            <div
              key={i}
              className="grid gap-4 px-4 py-2.5"
              style={{ gridTemplateColumns: gridCols }}
            >
              <div className="flex items-center justify-end">
                <div className="h-4 w-5 bg-th-surface-hover rounded" />
              </div>
              <div className="flex items-center gap-3 min-w-0">
                {showCover && (
                  <div className="w-10 h-10 shrink-0 rounded bg-th-surface-hover" />
                )}
                <div className="flex flex-col gap-1.5 min-w-0 flex-1">
                  <div className="h-4 w-3/5 bg-th-surface-hover rounded" />
                  <div className="h-3 w-2/5 bg-th-surface-hover/60 rounded" />
                </div>
              </div>
              {showArtist && (
                <div className="flex items-center">
                  <div className="h-3.5 w-3/5 bg-th-surface-hover/60 rounded" />
                </div>
              )}
              {showAlbum && (
                <div className="flex items-center">
                  <div className="h-3.5 w-3/5 bg-th-surface-hover/60 rounded" />
                </div>
              )}
              {showDateAdded && (
                <div className="flex items-center">
                  <div className="h-3.5 w-2/5 bg-th-surface-hover/60 rounded" />
                </div>
              )}
              <div className="flex items-center justify-end">
                <div className="h-3.5 w-8 bg-th-surface-hover/60 rounded" />
              </div>
              <div />
            </div>
          ))
        ) : virtualize ? (
          <VirtualTrackRows
            tracks={tracks}
            gridCols={gridCols}
            showCover={showCover}
            showArtist={showArtist}
            showAlbum={showAlbum}
            showDateAdded={showDateAdded}
            context={context}
            onPlay={onPlay}
            trackDisplayNumbers={trackDisplayNumbers}
            playlistId={playlistId}
            isUserPlaylist={isUserPlaylist}
            onTrackRemoved={onTrackRemoved}
            onAddToCurrentPlaylist={onAddToCurrentPlaylist}
            onLoadMore={onLoadMore}
            hasMore={hasMore}
            loadingMore={loadingMore}
          />
        ) : (
          tracks.map((track, index) => (
            <TrackRow
              key={`${track.id}-${index}`}
              track={track}
              index={index}
              displayNumber={trackDisplayNumbers?.[index]}
              gridCols={gridCols}
              showCover={showCover}
              showArtist={showArtist}
              showAlbum={showAlbum}
              showDateAdded={showDateAdded}
              context={context}
              onPlay={onPlay}
              playlistId={playlistId}
              isUserPlaylist={isUserPlaylist}
              onTrackRemoved={onTrackRemoved}
              onAddToCurrentPlaylist={onAddToCurrentPlaylist}
            />
          ))
        )}
      </div>

      {/* Infinite Scroll Sentinel (non-virtualized path only — virtualized
          path renders its own pagination skeletons inside VirtualTrackRows) */}
      {!virtualize && hasMore && (
        <div ref={sentinelRef}>
          {loadingMore ? (
            <div className="flex flex-col">
              {Array.from({ length: 5 }).map((_, i) => (
                <div
                  key={i}
                  className="grid gap-4 px-4 py-2.5"
                  style={{ gridTemplateColumns: gridCols }}
                >
                  <div className="flex items-center justify-end">
                    <div className="h-4 w-5 bg-th-surface-hover rounded" />
                  </div>
                  <div className="flex items-center gap-3 min-w-0">
                    {showCover && (
                      <div className="w-10 h-10 shrink-0 rounded bg-th-surface-hover" />
                    )}
                    <div className="flex flex-col gap-1.5 min-w-0 flex-1">
                      <div className="h-4 w-3/5 bg-th-surface-hover rounded" />
                      <div className="h-3 w-2/5 bg-th-surface-hover/60 rounded" />
                    </div>
                  </div>
                  {showArtist && (
                    <div className="flex items-center">
                      <div className="h-3.5 w-3/5 bg-th-surface-hover/60 rounded" />
                    </div>
                  )}
                  {showAlbum && (
                    <div className="flex items-center">
                      <div className="h-3.5 w-3/5 bg-th-surface-hover/60 rounded" />
                    </div>
                  )}
                  {showDateAdded && (
                    <div className="flex items-center">
                      <div className="h-3.5 w-2/5 bg-th-surface-hover/60 rounded" />
                    </div>
                  )}
                  <div className="flex items-center justify-end">
                    <div className="h-3.5 w-8 bg-th-surface-hover/60 rounded" />
                  </div>
                  <div />
                </div>
              ))}
            </div>
          ) : (
            <div className="h-8" />
          )}
        </div>
      )}
    </div>
  );
});

export default memo(function TrackList(props: TrackListProps) {
  // Virtual rows mount throughout a scroll. Keep the full navigation/favorite
  // action hooks at the list boundary instead of rebuilding them in every row.
  const { navigateToAlbum } = useNavigation();
  const {
    addFavoriteTrack,
    removeFavoriteTrack,
    addFavoriteVideo,
    removeFavoriteVideo,
  } = useFavoriteActions();
  const rowActions = useMemo(
    () => ({
      navigateToAlbum,
      addFavoriteTrack,
      removeFavoriteTrack,
      addFavoriteVideo,
      removeFavoriteVideo,
    }),
    [
      navigateToAlbum,
      addFavoriteTrack,
      removeFavoriteTrack,
      addFavoriteVideo,
      removeFavoriteVideo,
    ],
  );

  return (
    <TrackRowActionsContext.Provider value={rowActions}>
      <TrackListContent {...props} />
    </TrackRowActionsContext.Provider>
  );
});
