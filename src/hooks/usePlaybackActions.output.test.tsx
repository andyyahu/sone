import { act, cleanup, renderHook } from "@testing-library/react";
import { Provider, createStore } from "jotai";
import type { PropsWithChildren } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { usePlaybackActions } from "./usePlaybackActions";
import {
  acceptAudioOutputAtom,
  configuredAudioOutputAtom,
  type AudioOutputConfig,
} from "../atoms/audioOutput";
import {
  bitPerfectAtom,
  volumeAtom,
  volumeNormalizationAtom,
} from "../atoms/playback";
import { currentVideoAtom } from "../atoms/video";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("../contexts/ToastContext", () => ({
  useToast: () => ({ showToast: vi.fn() }),
}));
const native: AudioOutputConfig = {
  route: "native",
  exclusiveMode: false,
  bitPerfect: false,
  device: null,
  camillaConfig: null,
  hqplayerHost: "127.0.0.1",
  hqplayerPort: 4321,
};
function setup(configured = native, active = native) {
  const store = createStore();
  store.set(acceptAudioOutputAtom, {
    configured,
    active,
    playbackGeneration: 1,
    revision: 0,
    pending: configured !== active,
  });
  store.set(volumeAtom, 0.4);
  store.set(volumeNormalizationAtom, true);
  const wrapper = ({ children }: PropsWithChildren) => (
    <Provider store={store}>{children}</Provider>
  );
  return { store, ...renderHook(usePlaybackActions, { wrapper }) };
}
beforeEach(() => {
  vi.clearAllMocks();
  localStorage.clear();
  vi.mocked(invoke).mockResolvedValue(undefined);
});
afterEach(cleanup);

describe("effective output action guards", () => {
  it("blocks native gain actions while HQPlayer is active even after selecting native output", async () => {
    const { store, result } = setup(native, { ...native, route: "hqplayer" });
    await act(async () => {
      await result.current.setVolume(0.8);
      await result.current.setVolumeNormalization(false);
    });
    expect(store.get(volumeAtom)).toBe(0.4);
    expect(store.get(volumeNormalizationAtom)).toBe(true);
    expect(invoke).not.toHaveBeenCalled();
  });
  it("keeps current native gain controls enabled while HQPlayer is only pending", async () => {
    const { store, result } = setup({ ...native, route: "hqplayer" }, native);
    await act(async () => {
      await result.current.setVolume(0.8);
      await result.current.setVolumeNormalization(false);
    });
    expect(store.get(volumeAtom)).toBe(0.8);
    expect(store.get(volumeNormalizationAtom)).toBe(false);
    expect(invoke).toHaveBeenCalledWith("set_volume", { level: 0.8 });
    expect(invoke).toHaveBeenCalledWith("set_volume_normalization", {
      enabled: false,
    });
  });
  it("lets video volume change without sending gain to a locked audio output", async () => {
    const { store, result } = setup(native, { ...native, route: "hqplayer" });
    store.set(currentVideoAtom, { id: 10, title: "Video", duration: 100 });
    await act(async () => result.current.setVolume(0.8));
    expect(store.get(volumeAtom)).toBe(0.8);
    expect(invoke).not.toHaveBeenCalled();
  });
  it("enables bit-perfect on the next playback without rewriting remembered gain preferences", async () => {
    vi.mocked(invoke).mockImplementation(async (_command, args) => ({
      configured: (args as { config: AudioOutputConfig }).config,
      active: native,
      playbackGeneration: 1,
      revision: 0,
      pending: true,
    }));
    const { store, result } = setup();
    await act(async () => result.current.setBitPerfect(true));
    expect(store.get(configuredAudioOutputAtom)).toMatchObject({
      bitPerfect: true,
      exclusiveMode: true,
    });
    expect(store.get(volumeAtom)).toBe(0.4);
    expect(store.get(volumeNormalizationAtom)).toBe(true);
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke).toHaveBeenCalledWith("set_audio_output", {
      config: expect.objectContaining({ bitPerfect: true }),
    });
  });
  it("does not optimistically enable bit-perfect when saving fails", async () => {
    vi.mocked(invoke).mockRejectedValue(new Error("write failed"));
    const log = vi.spyOn(console, "error").mockImplementation(() => {});
    const { store, result } = setup();
    await act(async () => result.current.setBitPerfect(true));
    expect(store.get(bitPerfectAtom)).toBe(false);
    expect(store.get(volumeAtom)).toBe(0.4);
    expect(store.get(volumeNormalizationAtom)).toBe(true);
    log.mockRestore();
  });
});
