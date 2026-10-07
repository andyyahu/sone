import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { Provider, createStore } from "jotai";
import type { PropsWithChildren } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useAudioOutputActions, useAudioOutputSync } from "./useAudioOutput";
import {
  acceptAudioOutputAtom,
  audioOutputStateAtom,
  audioControlLockAtom,
  configuredAudioOutputAtom,
  type AudioOutputConfig,
  type AudioOutputState,
} from "../atoms/audioOutput";
import {
  bitPerfectAtom,
  volumeAtom,
  volumeNormalizationAtom,
} from "../atoms/playback";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));
const native: AudioOutputConfig = {
  route: "native",
  exclusiveMode: false,
  bitPerfect: false,
  device: null,
  camillaConfig: null,
  hqplayerHost: "127.0.0.1",
  hqplayerPort: 4321,
};
const hq = { ...native, route: "hqplayer" as const };
const state = (
  configured = native,
  active: AudioOutputConfig | null = native,
): AudioOutputState => ({
  configured,
  active,
  playbackGeneration: active ? 1 : null,
  revision: 0,
  pending: configured !== active && active !== null,
});
function useSyncedActions() {
  useAudioOutputSync();
  return useAudioOutputActions();
}
function setup(sync = false) {
  const store = createStore();
  store.set(acceptAudioOutputAtom, state());
  const rendered = vi.fn();
  const wrapper = ({ children }: PropsWithChildren) => (
    <Provider store={store}>{children}</Provider>
  );
  const useActions = sync ? useSyncedActions : useAudioOutputActions;
  const hook = renderHook(
    () => {
      rendered();
      return useActions();
    },
    { wrapper },
  );
  return { store, rendered, ...hook };
}
beforeEach(() => {
  vi.clearAllMocks();
  localStorage.clear();
  vi.mocked(listen).mockResolvedValue(vi.fn());
});
afterEach(cleanup);

describe("audio output preferences and effective controls", () => {
  it("uses the active output while a new route or bit-perfect preference is pending", () => {
    const { store } = setup();
    store.set(acceptAudioOutputAtom, state(hq, native));
    expect(store.get(audioControlLockAtom)).toBeNull();
    store.set(acceptAudioOutputAtom, state(native, hq));
    expect(store.get(audioControlLockAtom)).toBe("hqplayer");
    store.set(
      acceptAudioOutputAtom,
      state({ ...native, bitPerfect: true }, native),
    );
    expect(store.get(bitPerfectAtom)).toBe(true);
    expect(store.get(audioControlLockAtom)).toBeNull();
    store.set(
      acceptAudioOutputAtom,
      state(native, { ...native, route: "camilla", bitPerfect: true }),
    );
    expect(store.get(audioControlLockAtom)).toBeNull();
  });
  it("uses the configured output when idle", () => {
    const { store } = setup();
    store.set(acceptAudioOutputAtom, state(hq, null));
    expect(store.get(audioControlLockAtom)).toBe("hqplayer");
  });
  it("leaves preferences and remembered gain intact after a failed save", async () => {
    const { store, result } = setup();
    store.set(volumeAtom, 0.4);
    store.set(volumeNormalizationAtom, true);
    vi.mocked(invoke).mockRejectedValue(new Error("disk full"));
    await expect(
      result.current.setAudioOutput({ bitPerfect: true }),
    ).rejects.toThrow("disk full");
    expect(store.get(configuredAudioOutputAtom)).toEqual(native);
    expect(store.get(volumeAtom)).toBe(0.4);
    expect(store.get(volumeNormalizationAtom)).toBe(true);
  });
  it("serializes concurrent patches against the last acknowledged configuration", async () => {
    let finish!: (value: AudioOutputState) => void;
    vi.mocked(invoke).mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          finish = resolve;
        }),
    );
    vi.mocked(invoke).mockImplementationOnce(async (_command, args) =>
      state((args as { config: AudioOutputConfig }).config),
    );
    const { store, result } = setup();
    const first = result.current.setAudioOutput({ exclusiveMode: true });
    const second = result.current.setAudioOutput({ device: "hw:2" });
    await waitFor(() => expect(invoke).toHaveBeenCalledTimes(1));
    expect(store.get(configuredAudioOutputAtom).exclusiveMode).toBe(false);
    finish(state({ ...native, exclusiveMode: true }));
    await Promise.all([first, second]);
    expect(store.get(configuredAudioOutputAtom)).toMatchObject({
      exclusiveMode: true,
      device: "hw:2",
    });
  });
});

describe("audio output synchronization", () => {
  it("subscribes once without rendering on events and does not let hydration replace a newer event", async () => {
    let finish!: (value: AudioOutputState) => void;
    vi.mocked(invoke).mockImplementation(
      () =>
        new Promise((resolve) => {
          finish = resolve;
        }),
    );
    const unlisten = vi.fn();
    vi.mocked(listen).mockResolvedValue(unlisten);
    const { store, rendered, unmount } = setup(true);
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("get_audio_output"),
    );
    const count = rendered.mock.calls.length;
    const receive = vi.mocked(listen).mock.calls[0][1] as (event: {
      payload: AudioOutputState;
    }) => void;
    act(() => receive({ payload: { ...state(hq, hq), revision: 2 } }));
    await act(async () => finish({ ...state(), revision: 1 }));
    expect(store.get(audioOutputStateAtom)?.active?.route).toBe("hqplayer");
    expect(listen).toHaveBeenCalledTimes(1);
    expect(rendered).toHaveBeenCalledTimes(count);
    unmount();
    await waitFor(() => expect(unlisten).toHaveBeenCalledOnce());
  });
  it("ignores events delivered after unmount", async () => {
    vi.mocked(invoke).mockResolvedValue(state());
    const { store, unmount } = setup(true);
    await waitFor(() => expect(invoke).toHaveBeenCalled());
    unmount();
    const receive = vi.mocked(listen).mock.calls[0][1] as (event: {
      payload: AudioOutputState;
    }) => void;
    receive({ payload: state(hq, hq) });
    expect(store.get(configuredAudioOutputAtom).route).toBe("native");
  });
});

it("preserves a newer active-route event when an older save acknowledgement arrives", async () => {
  let finish!: (value: AudioOutputState) => void;
  vi.mocked(invoke).mockImplementation(async (command) =>
    command === "get_audio_output"
      ? state()
      : new Promise((resolve) => {
          finish = resolve;
        }),
  );
  const { store, result } = setup(true);
  await waitFor(() => expect(invoke).toHaveBeenCalledWith("get_audio_output"));
  const saving = result.current.setAudioOutput({ route: "hqplayer" });
  await waitFor(() =>
    expect(invoke).toHaveBeenCalledWith("set_audio_output", expect.anything()),
  );
  const receive = vi.mocked(listen).mock.calls[0][1] as (event: {
    payload: AudioOutputState;
  }) => void;
  act(() => {
    receive({ payload: { ...state(hq, native), revision: 1 } });
    receive({ payload: { ...state(hq, hq), revision: 2 } });
  });
  await act(async () => {
    finish({ ...state(hq, native), revision: 1 });
    await saving;
  });
  expect(store.get(audioOutputStateAtom)).toEqual({
    ...state(hq, hq),
    revision: 2,
  });
});

it("hydrates saved preferences before applying a partial update when root loading is pending", async () => {
  const { store, result } = setup();
  store.set(audioOutputStateAtom, null);
  const saved = {
    ...native,
    exclusiveMode: true,
    bitPerfect: true,
    device: "hw:4",
    camillaConfig: "/room.yml",
    hqplayerPort: 4322,
  };
  vi.mocked(invoke).mockImplementation(async (command, args) =>
    command === "get_audio_output"
      ? { ...state(saved), revision: 1 }
      : {
          ...state((args as { config: AudioOutputConfig }).config),
          revision: 2,
        },
  );
  await result.current.setAudioOutput({ route: "hqplayer" });
  expect(vi.mocked(invoke).mock.calls.map(([command]) => command)).toEqual([
    "get_audio_output",
    "set_audio_output",
  ]);
  expect(store.get(configuredAudioOutputAtom)).toEqual({
    ...saved,
    route: "hqplayer",
  });
});

it("does not save placeholder preferences if initial hydration fails", async () => {
  const { store, result } = setup();
  store.set(audioOutputStateAtom, null);
  vi.mocked(invoke).mockRejectedValue(new Error("settings unreadable"));
  await expect(
    result.current.setAudioOutput({ route: "hqplayer" }),
  ).rejects.toThrow("settings unreadable");
  expect(invoke).toHaveBeenCalledTimes(1);
  expect(invoke).toHaveBeenCalledWith("get_audio_output");
  expect(store.get(audioOutputStateAtom)).toBeNull();
});

it("ignores older configured and active snapshots from any delivery channel", () => {
  const { store } = setup();
  store.set(acceptAudioOutputAtom, { ...state(hq, hq), revision: 3 });
  store.set(acceptAudioOutputAtom, { ...state(native, native), revision: 2 });
  expect(store.get(audioOutputStateAtom)).toEqual({
    ...state(hq, hq),
    revision: 3,
  });
  expect(store.get(audioControlLockAtom)).toBe("hqplayer");
});
