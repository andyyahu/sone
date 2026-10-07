import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { cleanup, renderHook, waitFor } from "@testing-library/react";
import { createStore, Provider } from "jotai";
import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { useGaplessPrefetch } from "./useGaplessPrefetch";
import { currentTrackAtom, queueAtom, gaplessAtom } from "../atoms/playback";
import { PROXY_SAVED_EVENT } from "../atoms/proxy";
import {
  acceptAudioOutputAtom,
  configuredAudioOutputAtom,
} from "../atoms/audioOutput";
import type { Track } from "../types";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

// Not automatic: this suite runs without vitest globals, so React Testing
// Library never registers its own cleanup. Without this every hook a test
// mounts stays mounted — and every one of them answers the window event the
// last test dispatches, counted against the same `invoke` mock.
afterEach(cleanup);

const DEAD = { id: 42, title: "dead", _qid: "q42" } as unknown as Track;
const LIVE = { id: 43, title: "live", _qid: "q43" } as unknown as Track;

function setup(store: ReturnType<typeof createStore>, predict: () => Track) {
  const pendingNextRef = { current: null };
  const wrapper = ({ children }: { children: React.ReactNode }) =>
    React.createElement(Provider, { store }, children);
  return renderHook(
    () => useGaplessPrefetch(predict, pendingNextRef as never),
    {
      wrapper,
    },
  );
}

function attempts() {
  return vi.mocked(invoke).mock.calls.filter((c) => c[0] === "set_next_track");
}

describe("useGaplessPrefetch failure memo", () => {
  beforeEach(() => vi.mocked(invoke).mockReset());

  it("does not re-invoke set_next_track for a track that just failed", async () => {
    const store = createStore();
    store.set(gaplessAtom, true);
    store.set(currentTrackAtom, { id: 1 } as unknown as Track);
    vi.mocked(invoke).mockImplementation(async (cmd: string) => {
      if (cmd === "get_gapless_supported") return true;
      if (cmd === "set_next_track")
        throw { kind: "Api", message: { status: 429, body: "" } };
      return undefined;
    });

    setup(store, () => DEAD);
    await waitFor(() => expect(attempts()).toHaveLength(1));

    // Simulate the background paginator: one queue write per page, spaced
    // wider than the 250ms debounce so each is a distinct refresh.
    for (let i = 0; i < 5; i++) {
      store.set(queueAtom, [DEAD, ...store.get(queueAtom)]);
      await new Promise((r) => setTimeout(r, 300));
    }

    expect(attempts()).toHaveLength(1);
  });

  it("still attempts a DIFFERENT next track while one is memoized", async () => {
    const store = createStore();
    store.set(gaplessAtom, true);
    store.set(currentTrackAtom, { id: 1 } as unknown as Track);
    let next: Track = DEAD;
    vi.mocked(invoke).mockImplementation(
      async (cmd: string, args?: unknown) => {
        if (cmd === "get_gapless_supported") return true;
        if (cmd === "set_next_track") {
          if ((args as { trackId: number }).trackId === DEAD.id)
            throw { kind: "Api", message: { status: 429, body: "" } };
          return {};
        }
        return undefined;
      },
    );

    setup(store, () => next);
    await waitFor(() => expect(attempts()).toHaveLength(1));

    next = LIVE;
    store.set(queueAtom, [LIVE, ...store.get(queueAtom)]);
    await waitFor(() => expect(attempts()).toHaveLength(2));
    expect(attempts()[1][1]).toMatchObject({ trackId: LIVE.id });
  });
});

describe("useGaplessPrefetch proxy invalidation", () => {
  beforeEach(() => vi.mocked(invoke).mockReset());

  it("re-arms the slot after a proxy save, whose prediction is unchanged", async () => {
    const store = createStore();
    store.set(gaplessAtom, true);
    store.set(currentTrackAtom, { id: 1 } as unknown as Track);
    vi.mocked(invoke).mockImplementation(async (cmd: string) => {
      if (cmd === "get_gapless_supported") return true;
      if (cmd === "set_next_track") return {};
      return undefined;
    });

    setup(store, () => LIVE);
    await waitFor(() => expect(attempts()).toHaveLength(1));

    // The dedup this has to defeat: an ordinary refresh with the same
    // prediction sends nothing, which is why the backend detaching the branch
    // would otherwise cost one audible gap.
    store.set(queueAtom, [LIVE, ...store.get(queueAtom)]);
    await new Promise((r) => setTimeout(r, 300));
    expect(attempts()).toHaveLength(1);

    window.dispatchEvent(new Event(PROXY_SAVED_EVENT));
    await waitFor(() => expect(attempts()).toHaveLength(2));
    expect(attempts()[1][1]).toMatchObject({ trackId: LIVE.id });
  });
});

describe("output-aware gapless prefetch", () => {
  beforeEach(() => {
    vi.mocked(invoke).mockReset();
    vi.mocked(invoke).mockImplementation(async (command) =>
      command === "get_gapless_supported" ? true : {},
    );
  });
  it("prepares HQPlayer even when the remembered native mode is exclusive and bit-perfect", async () => {
    const store = createStore();
    const config = {
      ...store.get(configuredAudioOutputAtom),
      route: "hqplayer" as const,
      exclusiveMode: true,
      bitPerfect: true,
    };
    store.set(acceptAudioOutputAtom, {
      configured: config,
      active: config,
      playbackGeneration: 1,
      revision: 0,
      pending: false,
    });
    store.set(currentTrackAtom, LIVE);
    setup(store, () => DEAD);
    await waitFor(() => expect(attempts()).toHaveLength(1));
  });
  it.each(["camilla", "pending"])(
    "does not arm a next track for %s output",
    async (mode) => {
      const store = createStore();
      const config = {
        ...store.get(configuredAudioOutputAtom),
        route:
          mode === "camilla" ? ("camilla" as const) : ("hqplayer" as const),
      };
      store.set(acceptAudioOutputAtom, {
        configured: config,
        active: config,
        playbackGeneration: 1,
        revision: 0,
        pending: mode === "pending",
      });
      store.set(currentTrackAtom, LIVE);
      setup(store, () => DEAD);
      await waitFor(() =>
        expect(invoke).toHaveBeenCalledWith("clear_next_track"),
      );
      expect(attempts()).toHaveLength(0);
    },
  );
  it("invalidates in-flight preparation when a route change becomes pending", async () => {
    let finish!: (value: unknown) => void;
    vi.mocked(invoke).mockImplementation(async (command) =>
      command === "set_next_track"
        ? new Promise((resolve) => {
            finish = resolve;
          })
        : true,
    );
    const store = createStore();
    const config = store.get(configuredAudioOutputAtom);
    store.set(acceptAudioOutputAtom, {
      configured: config,
      active: config,
      playbackGeneration: 1,
      revision: 0,
      pending: false,
    });
    store.set(currentTrackAtom, LIVE);
    setup(store, () => DEAD);
    await waitFor(() => expect(attempts()).toHaveLength(1));
    store.set(acceptAudioOutputAtom, {
      configured: { ...config, route: "hqplayer" },
      active: config,
      playbackGeneration: 1,
      revision: 0,
      pending: true,
    });
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("clear_next_track"),
    );
    finish({});
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(attempts()).toHaveLength(1);
  });
});
