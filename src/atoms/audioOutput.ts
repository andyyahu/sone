import { atom } from "jotai";
import {
  bitPerfectAtom,
  exclusiveModeAtom,
  exclusiveDeviceAtom,
} from "./playback";

export interface AudioOutputConfig {
  route: "native" | "camilla" | "hqplayer";
  exclusiveMode: boolean;
  bitPerfect: boolean;
  device: string | null;
  camillaConfig: string | null;
  hqplayerHost: string;
  hqplayerPort: number;
}
export interface AudioOutputState {
  revision: number;
  playbackGeneration: number | null;
  configured: AudioOutputConfig;
  active: AudioOutputConfig | null;
  pending: boolean;
}
export const audioBufferingAtom = atom<{
  trackId: number;
  playbackGeneration: number;
  wasPlaying: boolean;
} | null>(null);
export const audioOutputStateAtom = atom<AudioOutputState | null>(null);
export const configuredAudioOutputAtom = atom<AudioOutputConfig>(
  (get) =>
    get(audioOutputStateAtom)?.configured ?? {
      route: "native",
      exclusiveMode: get(exclusiveModeAtom),
      bitPerfect: get(bitPerfectAtom),
      device: get(exclusiveDeviceAtom),
      camillaConfig: null,
      hqplayerHost: "127.0.0.1",
      hqplayerPort: 4321,
    },
);
export const effectiveAudioOutputAtom = atom(
  (get) => get(audioOutputStateAtom)?.active ?? get(configuredAudioOutputAtom),
);
export const audioControlLockAtom = atom(
  (get): "hqplayer" | "bit-perfect" | null => {
    const config = get(effectiveAudioOutputAtom);
    if (config.route === "hqplayer") return "hqplayer";
    return config.route === "native" && config.bitPerfect
      ? "bit-perfect"
      : null;
  },
);
export const effectiveBitPerfectAtom = atom(
  (get) => get(audioControlLockAtom) === "bit-perfect",
);
export const acceptAudioOutputAtom = atom(
  null,
  (get, set, state: AudioOutputState) => {
    const current = get(audioOutputStateAtom);
    if (current && state.revision < current.revision) return;
    set(audioOutputStateAtom, state);
    set(exclusiveModeAtom, state.configured.exclusiveMode);
    set(bitPerfectAtom, state.configured.bitPerfect);
    set(exclusiveDeviceAtom, state.configured.device);
  },
);
