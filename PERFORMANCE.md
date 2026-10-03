# UI performance experiments

## Implemented changes

- Media grids with at least 40 cards render a window of rows. Responsive
  columns, variable card heights, page scroll restoration, and pagination use
  the existing page scroll container. Small shelves retain normal CSS layout.
- Sidebar collections window their 56px rows, retaining focused rows and their
  neighbours for keyboard navigation. Artist track pages enable the existing
  track-list virtualizer. Stable item-key callbacks let track/sidebar scrolling
  reuse measured geometry instead of recalculating the entire collection.
- Uncached `TidalImage` covers wait for viewport intersection through a shared
  observer. Image IPC requests are deduplicated and limited to six concurrent
  requests; image decoding is asynchronous. Queued requests with no remaining
  consumers are cancelled when navigating away; visible covers take priority
  over speculative preloads. Running IPC finishes into the cache. Map insertion
  order tracks LRU eviction without sorting the cache.
- Native `MediaCard` URLs and `TidalImage` Blob URLs share a browser admission
  queue: at most four pending image loads/decodes and two new starts per frame.
  A native URL keeps its original transport. Cancellation releases queued and
  active slots, and already displayed sources can start before paint when the
  same budgets allow it.
- Image visibility observers are shared per page scroll container, with a 400px
  preload margin and viewport fallback for portals or consumers outside a page.
  A viewport-only observer cannot extend an inner scroller's clip with
  `rootMargin`, as specified by [Intersection Observer](https://www.w3.org/TR/intersection-observer/).
- Playlist play/removal callbacks remain stable while pages load; playback reads
  the latest committed collection. Library card elements are reused while only
  loading flags change. Bulk skeletons are static, and media skeletons reserve
  the height of an ordinary one-line card before artwork arrives.
- Track rows share favorite actions at the list boundary instead of mounting an
  authentication subscription and every favorite-action hook per row. The narrow
  context remains stable across same-account token refreshes. Navigation actions
  are shared per Jotai store, including artist links in newly mounted rows.
- Older date-added labels reuse one locale formatter across virtual-row mounts.
  Relative day labels are still recalculated from the current time; missing or
  invalid dates keep their previous display behavior.
- Closed, covered, or document-hidden players pause motion covers and lyric
  animation loops. Unopened motion covers do not receive a video source. Tilt
  effects release their GPU resources while inactive and can initialize again
  when reopened.
- Mini-player position lives outside the main component tree. Only its progress
  bar updates, four times per second while playing, visible, and hovered or
  dragged. Compact/narrow layouts run no progress timer. Position-only IPC
  heartbeats re-anchor the clock without rendering artwork or controls.
- Main/fullscreen audio scrubbers stop polling while covered or hidden. Video
  scrubbers use native media events plus a 150ms timer only while visible and
  playing, replacing their duplicate animation-frame loops. Revealing controls
  or seeking while paused immediately refreshes progress.
- Track rows subscribe only to their own favorite boolean; action-only callers
  no longer subscribe to all six favorite collections. Toast actions retain a
  stable context value, so showing or dismissing a notification does not render
  every song row that can trigger one.
- Queue persistence coalesces edits before sanitizing and serializing, with a
  200ms delay followed by idle time (up to one second). Repeated track references
  are sanitized once per capture. Local snapshots flush on hiding/unmount;
  backend writes are deduplicated and serialized. Recovery markers preserve
  newer local state and logout tombstones until backend saves finish.
- Concurrent frontend API cache misses share one IPC request per key.
  Invalidation, optimistic mutations, and logout detach older requests, so late
  responses cannot overwrite newer cached data.
- Native cache removals use each entry's tags and hash sets instead of scanning
  every cached image. Expired entries skip disk reads and decryption. Image HTTP
  errors are rejected before caching, so an error page cannot become a long-lived
  broken cover.

## Validation and local measurements

Validated on 2026-09-27: 533 frontend tests across 73 files and 362 Rust tests
pass, including regressions for cancellation, shared image requests, favorite
row render counts, mini-player lifecycle, and persistence recovery. Rust socket tests need an
environment that permits local TCP and Unix sockets. Changed frontend files
pass ESLint and Prettier; changed Rust files pass rustfmt.
Production TypeScript/Vite and Tauri release builds also pass. The executable
is `src-tauri/target/release/sone`; fully quit the old process before launching it.

The earlier repository-wide checks reported an ESLint error in `PlaylistView.tsx`,
fixed in the loading follow-up below. Remaining pre-existing issues include
14 Clippy diagnostics promoted to errors outside the modified logic; formatting
drift and existing Knip findings. These were not used to weaken validation of
the modified files.

Deterministic regression tests verify that 100 rapid queue edits produce one
capture, 100 abandoned queued images produce no IPC calls, unrelated favorite
changes render no track rows, and mini-player ticks/position-only heartbeats
leave artwork and controls unchanged. These are work-count checks; they do not
measure frame rate or end-to-end latency on a real library.

Additional regressions measure three scroll events over 1,000 sidebar items:
full-list key reads fell from 3,000 to zero, while reordered rows retain their
identity. One hundred concurrent identical API misses use one IPC call. Showing,
dismissing, and expiring toasts produce zero renders of 100 action consumers.

A local production-built synthetic page on 2026-09-26 compared 1,000 `MediaCard`
components with the original CSS grid and the new `MediaGrid`. It used
WebKitGTK 2.52.6 on Hyprland/Wayland, the same captured viewport
(1706 × 910 pixels), generated gradient covers, and 240 animation frames of scripted scrolling. Each scenario
was run once in a fresh WebKit context; real network images, playback, and
account data were not involved.

| Scenario | Mounted cards | DOM elements | Median frame | p95 frame | Frames >32ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| Original grid, DMA-BUF disabled | 1,000 | 13,013 | 17ms | 24ms | 0 |
| Virtual grid, DMA-BUF disabled | 18 | 252 | 17ms | 20ms | 1 |
| Virtual grid, DMA-BUF allowed | 18 | 252 | 17ms | 17ms | 1 |

Both virtual-grid runs had zero overlapping rows, and the original/virtual
screenshots showed matching visible card layout. DOM size dropped by about
98%; the frame-time samples are exploratory, not a statistically established
speedup or a guarantee for a real library. Virtual scroll height is estimated
for unseen rows and is refined as their content is measured.

## 2026-09-28: loading while scrolling

The loading follow-up limits browser image work as well as IPC, keeps existing
rows/cards stable during request-state changes, and removes bulk skeleton pulse
animations. Native image sources remain native URLs. Four load/decode slots and
two starts per frame spread completion work without waiting for scrolling to stop.
Image source ownership is imperative inside `ScheduledImage`, so cancellation and
StrictMode replay cannot leave React's source state out of sync with the DOM.

A first iteration revealed that viewport-only image observation delayed covers
inside the page's overflow container. Observing the actual page scroller restores
the intended 400px preload margin. Each scroller shares one observer; portals use
the viewport if the inherited page root does not contain their DOM element.

The local `nocommit/loading-perf.tsx` fixture uses production React and actual
MediaGrid/MediaCard components. Over four seconds it appends eight 60-card pages
to 120 initial cards and scrolls 3,600px, then repeats after warming the images.
It uses deterministic 512px PNGs and simulated local delays. Benchmark v3 clips
visible-cover accounting to both the page scroller and the WebView viewport.
Both revisions used WebKitGTK 2.52.6, effective acceleration `Always`, the same
853 × 920 CSS-pixel WebView at DPR 2, and the same 1080 × 650 content scroller
(partly clipped horizontally by the tiled window).

Two native-image runs per revision, with pending-card skeletons enabled:

| Loading phase | Before, runs 1 / 2 | Final changes, runs 1 / 2 |
| --- | --- | --- |
| p95 animation-frame interval | 35 / 37ms | 26 / 24ms |
| Intervals over 32ms | 27 / 31 | 7 / 0 |
| Unique covers loaded by phase end | 86 / 86 | 86 / 86 |
| Pending visible covers, every sampled point and phase end | 0 / 0 | 0 / 0 |
| Source-ready to first DOM load, median | 22 / 22ms | 37 / 38ms |
| Source-ready to first DOM load, p95 | 53 / 56ms | 90 / 99ms |

The admission budget adds a small cover delay; it does not make every image
load faster. Prefetching kept that delay outside all sampled visible regions
in these runs. Both revisions had zero sampled row overlaps. Warm-phase p95
intervals were 25 / 23ms before and 24 / 21ms after.

One additional before/after pair used real TidalImage caching and its six-request
IPC scheduler, with only the binary bridge reply mocked. The loading phase
displayed 61 versus 85 unique covers, p95 intervals were 37 versus 33ms, and
intervals over 32ms were 17 versus 15. Average sampled pending visible covers
fell from 4.71 to 3.71; the peak rose from 9 to 11. This path still waits for
simulated downloads and does not establish a large frame-time improvement.

These are exploratory animation-frame measurements, not compositor FPS, native
wheel-input latency, real network contention, or a guarantee for an account's
library. Native mode delays assigning local Blob URLs instead of measuring HTTP.
Readiness totals are cumulative; only loading-phase source-to-load latency is
used above. Some earlier probe shutdowns printed allocator diagnostics after
complete results; the final per-root and Tidal probes exited cleanly.

Local fixture instructions are in `nocommit/loading-perf-README.md`. Raw final
results are `nocommit/loading-native-v3-before-{1,2}.jsonl`,
`nocommit/loading-native-v3-root-{1,2}.jsonl`, and
`nocommit/loading-tidal-v3-{before,after}.jsonl`.

Follow-up validation: all 554 frontend tests across 77 files pass. New regressions
cover zero existing-row renders during pending pagination, latest-queue playback,
image start/decode budgets, cancellation, StrictMode, and per-scroller observation.
TypeScript, changed-file formatting, and the Tauri release build pass. Frontend
ESLint has zero errors (existing warnings remain). The rebuilt executable is
`src-tauri/target/release/sone`, produced on 2026-09-28 at 07:48 +0800. Rust logic
was unchanged during this loading follow-up; earlier Rust results remain above.

## 2026-09-28: already-loaded playlist rows

Follow-up feedback reported good home/feed behavior but uneven playlist scrolling
even after all content loaded. This change reduces row-mount work:

- Favorite actions move to one stable context per TrackList. A regression with
  20 rows observes one authentication subscription, and a same-account token
  refresh renders no rows. Account changes still update audio/video favorite
  operations to the new user ID.
- All 16 navigation actions are shared per Jotai store, including the artist
  links used by new virtual rows. Tests preserve history stamping, overlay
  dismissal, remount identity, and isolation when a Provider changes stores.
- Date-added labels share a lazily created Intl formatter. Relative-date
  boundaries, invalid dates, and the default locale retain their prior behavior.

A Node v26 microbenchmark of 1,000 old-date labels took 27.81 / 37.41 / 43.48ms
with the previous formatter and 1.08 / 0.92 / 2.97ms with the shared formatter.
This measures date formatting only; it is not a browser frame-rate result.

The production `nocommit/playlist-perf.tsx` fixture mounts actual TrackList rows
and hooks with 1,000 tracks, old date-added labels, and 160px covers. It preloads
160 covers and visits the same path before measuring four seconds over 6,000px.
Two alternating runs per revision used the same WebKitGTK 2.52.6 window and
effective `Always` policy:

| Warm direct-list measurement | Before, runs 1 / 2 | After, runs 1 / 2 |
| --- | --- | --- |
| p95 animation-frame interval | 19 / 17ms | 17 / 17ms |
| Intervals over 32ms | 0 / 0 | 0 / 0 |
| New row mounts while scrolling | 100 / 100 | 100 / 100 |
| Image-byte IPC requests while scrolling | 0 / 0 | 0 / 0 |
| Frames with uncovered rows or row overlaps | 0 / 0 | 0 / 0 |

At most 31 rows were mounted. No sampled visible cover was pending. The baseline
is already smooth in this synthetic case, so these timings do not establish that
the user's remaining native-input stutter is fixed. They verify the same visible
workload without hiding rows or delaying network work. A fixture-only experiment
disabling synchronous virtualizer commits had p95 25ms and was not adopted.

Adding the page's gradient, PageContainer, padding, and inner overflow wrapper
also produced p95 17ms before and after in one pair. Removing that overflow only
in the fixture did not improve its p95, so the page structure remains unchanged.
At 150% root CSS zoom, a correctly sized 527 × 529 layout-pixel scroller also
reported p95 17ms on both revisions, no pending visible covers or uncovered rows,
and zero timed IPC calls. These v3 results are in
`nocommit/playlist-v3-{before,after}-zoom-correct.jsonl`; earlier v2 zoom runs
double-scaled viewport units and are not used for the comparison.
These are scripted scroll intervals with per-frame geometry sampling, not GPU
presentation or wheel-input latency in a real account. Reproduction instructions
are in `nocommit/playlist-perf-README.md`; direct-list raw results are
`nocommit/playlist-warm-{before,after}-{1,2}.jsonl`.

Validation: all 564 frontend tests across 78 files pass, as do TypeScript,
changed-file Prettier checks, and the Tauri release build. Changed-file ESLint
reports zero errors and the existing TanStack/React Compiler warning. The
release executable at `src-tauri/target/release/sone` includes these changes.
This round changes no Rust logic; no additional Rust test run was needed.

## Backscroll and slow image admission (2026-09-29)

The shared browser-image queue previously let four unfinished new covers block
artwork that had already displayed. The ready-source marker only allowed an
early start when the same FIFO and all its slots were free.

The shipped change separates returning and new covers, with a combined maximum
of six active loads/decodes. Each group can use four slots when alone, leaving
two available to the other group under contention. Both still share the same
two-starts-per-frame limit, and alternate when both can progress. A previously
displayed cover still calls native `decode()` because WebKit may have discarded
its decoded surface. Cancellation and errors release their own slot once.
Tests cover both directions of starvation, fairness, cancellation, the shared
limits, and a StrictMode remount while four cold decodes remain unresolved.

A separate WebKitGTK 2.52.6 probe uses the real ScheduledImage, native HTTP
loading, and native decoding. A loopback server delays four new PNGs by two
seconds, while an already decoded source unmounts and returns:

| Native image-return measurement | Before | Queue-only change |
| --- | --- | --- |
| Returning cached image's load event | 1,984ms | 3ms |
| First cold image completes after remount | 1,974ms | 1,973ms |
| Returning image loads before cold completion | No | Yes |

Both runs verified all four cold sources were assigned before remount and
remained unfinished for over 1,800ms. The server log confirms real request
arrivals; neither decode nor load events are mocked. This demonstrates removal
of queue blocking under controlled latency, not an actual-library FPS gain or
GPU presentation latency. See `nocommit/warm-return-perf-README.md` and
`nocommit/warm-return-{before,after}.jsonl`.

The final queue-only backscroll comparison used matching full-frame sampling,
WebKitGTK 2.52.6, effective `Always` acceleration, and an 853 × 920 CSS-pixel
window. Immediate-return p95 was 33ms before / 28ms after; settled-return p95 was
19ms in both runs. Each return still mounted 91 rows, as expected for unchanged
virtualization. Both versions had zero sampled row gaps/overlaps and zero pending
visible covers after settling. Cold-scroll p95 varied substantially even between
baseline runs (39ms and 19ms); the queue-only runs were 35ms and 39ms. These
samples do not establish a general frame-rate gain. All phase results remain in
`nocommit/backscroll-shipping-{before-v3,queue-only}-{0,1}.jsonl`.

Full-suite validation also exposed a lyric-scroll listener race: the effect
could run before the loading placeholder became a scroll container. Binding the
same handler directly to that container closes the gap. Sidebar tests now drain
the virtualizer's debounced scroll notification before jsdom teardown, preventing
a late callback from observing a destroyed window.

Validation: all 569 frontend tests across 78 files pass. TypeScript and
changed-file ESLint pass with zero errors; the drawer retains its pre-existing
lint warnings. Changed-file formatting and the Tauri release build also pass.
The executable at `src-tauri/target/release/sone` was rebuilt on 2026-09-29 at
09:52 Asia/Taipei (42,032,032 bytes). No Rust logic changed in this round.

### Retention experiments rejected

Several implementations kept up to 96 previously rendered rows: hidden DOM,
CSS containment/visibility variants, and detached portal hosts with a memoized
row boundary. The latter kept at most 31 rows attached in the test viewport and
reduced a settled return from 91 React row mounts to 26, and explicit decode
calls from 92 to 28. However, its settled-return p95 frame interval was 25ms
against the baseline's 18ms; other phases also varied. Reduced operation counts
did not establish a smoother result. None of these retention changes or their
image-parking API is included in the release.

The final prototype remains in the ignored
`nocommit/backscroll-portal-experiment-src/` snapshot. Raw comparisons and
reproduction instructions live in `nocommit/backscroll-perf-README.md`.
The fixture uses actual production TrackList rows, 1,000 tracks, unique 160px
rasters, and 180–355ms simulated image-byte IPC. It measures cold downward
scrolling, immediate/settled return, and repeated down/up traversal. It counts
placeholders without an img as pending and distinguishes React mounts from DOM
reattachment. Default mode checks geometry every frame; optional lean mode
samples every eight frames. These modes must not be pooled for frame comparisons.

These are controlled JavaScript frame intervals and explicit decoder calls,
not native wheel latency, compositor presentation times, or real-library FPS.
Instrumentation cost also changes with DOM size; sparse sampling can miss brief
blanks. The queue-only release preserves the existing list rendering and its
memory bounds.

## Compare WebKit renderer policies

### 2026-09-28: remove the blanket NVIDIA workaround

After real-use feedback still reported choppy scrolling, the default launch
policy was changed. Loading an NVIDIA module no longer disables DMA-BUF;
`auto` now defers to WebKit, including on Intel/NVIDIA hybrid machines.

On this system's WebKitGTK 2.52.6, the old `WEBKIT_DISABLE_DMABUF_RENDERER=1`
setting makes the accelerated backing store fail its requirements check, which
disables hardware acceleration. This was verified against the versioned
[backing-store source](https://github.com/WebKit/WebKit/blob/webkitgtk-2.52.6/Source/WebKit/UIProcess/gtk/AcceleratedBackingStore.cpp)
and [hardware acceleration manager](https://github.com/WebKit/WebKit/blob/webkitgtk-2.52.6/Source/WebKit/UIProcess/gtk/HardwareAccelerationManager.cpp),
then confirmed locally by reading back the effective policy: `Never` with the
old override, `Always` with it unset. Requesting `OnDemand` does not prove it is
the effective policy, so the app now logs that readback after applying settings.

An isolated GTK3/WebKit window ran the same production-built 1,000-card virtual
grid, with an ephemeral context and 240 scripted scroll frames. Other compositing
overrides were unset. Two alternating old/new runs produced:

| Launch mode | Effective policy | Median interval, runs 1 / 2 | p95 interval, runs 1 / 2 | Intervals >32ms, runs 1 / 2 |
| --- | --- | --- | --- | --- |
| Old NVIDIA workaround (`=1`) | Never | 35 / 47ms | 53 / 51ms | 153 / 239 |
| New auto (unset) | Always | 17 / 17ms | 19 / 22ms | 0 / 1 |

Both modes mounted 24 cards and 332 DOM elements, with identical scroll extents
and zero row overlaps. Run 2 recorded the same 853 × 920 CSS-pixel viewport,
device-pixel ratio 2, and 120Hz monitor in both modes; its screenshots matched.
These are JavaScript animation-frame intervals for synthetic, programmatic
scrolling, not compositor presentation times or proof of 120fps native wheel
scrolling. Actual account pages, network art and music video playback still need
user validation. No SONE process was running during the diagnostic probes, so
the earlier user session's effective policy could not be directly inspected.

Follow-up validation: all five renderer-policy tests and all 12 source-guard
tests pass. TypeScript/Vite and the Tauri release build pass; the rebuilt
`src-tauri/target/release/sone` includes the new default and policy readback.

### Launch comparisons

`SONE_RENDERER` applies to one SONE process at startup. Fully quit SONE from the
tray before each run: launching a second instance only activates the existing
window, whose renderer is already initialized. Use the same release binary,
window size, display scaling, page, and playlist for each comparison.

| Profile | Behavior when `WEBKIT_DISABLE_DMABUF_RENDERER` is unset |
| --- | --- |
| `auto` (default) | Leaves WebKit's choice alone, regardless of loaded NVIDIA modules. |
| `dmabuf` | Sets `WEBKIT_DISABLE_DMABUF_RENDERER=0`, allowing WebKit's DMA-BUF path even when NVIDIA modules are loaded. |
| `compatibility` | Sets `WEBKIT_DISABLE_DMABUF_RENDERER=1`, disabling the DMA-BUF renderer. |

On Intel/NVIDIA hybrid systems, a loaded NVIDIA module does not identify the GPU
rendering the window. Neither `auto` nor `dmabuf` selects a GPU or guarantees
hardware acceleration: other environment settings and driver capabilities still
apply. `compatibility` reproduces the old override for comparison or fallback.

After building with `pnpm tauri build --no-bundle`, compare:

```bash
SONE_RENDERER=auto ./src-tauri/target/release/sone
SONE_RENDERER=dmabuf ./src-tauri/target/release/sone
SONE_RENDERER=compatibility ./src-tauri/target/release/sone
```

For a direct Cargo release build, run `pnpm build` first, then
`cargo build --manifest-path src-tauri/Cargo.toml --release --locked --features tauri/custom-protocol`.
The `tauri/custom-protocol` feature embeds the frontend instead of expecting the
Vite development server.

Each launch prints the selected policy and relevant `WEBKIT_*` values to stderr
before GTK starts, then the main WebView prints its effective hardware policy
and smooth-scrolling setting. `Some("1")` means the variable is set; `None` means unset. A
pre-existing `WEBKIT_DISABLE_DMABUF_RENDERER` wins over every profile, including
an empty value. Other WebKit variables remain untouched and can still constrain
compositing. For a comparison without inherited renderer overrides, use:

```bash
env -u WEBKIT_DISABLE_DMABUF_RENDERER \
    -u WEBKIT_DISABLE_COMPOSITING_MODE \
    -u WEBKIT_FORCE_COMPOSITING_MODE \
    -u WEBKIT_DMABUF_RENDERER_FORCE_SHM \
    SONE_RENDERER=dmabuf ./src-tauri/target/release/sone
```

Repeat with `SONE_RENDERER=auto`. Scroll the same long list, open and close the
player drawer, resize the window, and test music video playback. Record visible
stutter, CPU usage, any blank frames, the startup policy line, WebKitGTK/driver
versions, Wayland/X11 session, monitor scaling, and whether the window was on an
external display. Treat frame-rate improvements as unverified until measured on
the affected machine.

To reproduce the previous default on NVIDIA systems, fully quit and launch
with `SONE_RENDERER=compatibility`; no settings file is changed. Use this fallback
if the new default produces a blank window or artifacts. The historical reason
for the old workaround is [WebKit bug 261874](https://bugs.webkit.org/show_bug.cgi?id=261874).
