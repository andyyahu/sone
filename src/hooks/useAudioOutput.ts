import { useCallback, useEffect } from "react";
import { useStore } from "jotai";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  acceptAudioOutputAtom,
  audioOutputStateAtom,
  configuredAudioOutputAtom,
  type AudioOutputConfig,
  type AudioOutputState,
} from "../atoms/audioOutput";

/** One listener at the app root; output changes never subscribe the app tree. */
export function useAudioOutputSync() {
  const store = useStore();
  useEffect(() => {
    let disposed = false;
    const subscription = listen<AudioOutputState>(
      "audio-output-changed",
      ({ payload }) => {
        if (disposed) return;
        store.set(acceptAudioOutputAtom, payload);
      },
    );
    void subscription
      .then(async () => {
        const state = await invoke<AudioOutputState>("get_audio_output");
        if (!disposed) store.set(acceptAudioOutputAtom, state);
      })
      .catch((error) => console.error("Failed to load audio output:", error));
    return () => {
      disposed = true;
      void subscription.then((unlisten) => unlisten()).catch(() => {});
    };
  }, [store]);
}

const saves = new WeakMap<ReturnType<typeof useStore>, Promise<unknown>>();

export function useAudioOutputActions() {
  const store = useStore();
  const setAudioOutput = useCallback(
    (patch: Partial<AudioOutputConfig>) => {
      const previous = saves.get(store) ?? Promise.resolve();
      const operation = previous
        .catch(() => {})
        .then(async () => {
          // Initial UI defaults are only placeholders. Merge partial edits
          // against persisted preferences even if root hydration is still pending.
          if (!store.get(audioOutputStateAtom)) {
            const initial = await invoke<AudioOutputState>("get_audio_output");
            store.set(acceptAudioOutputAtom, initial);
          }
          const config = { ...store.get(configuredAudioOutputAtom), ...patch };
          const state = await invoke<AudioOutputState>("set_audio_output", {
            config,
          });
          store.set(acceptAudioOutputAtom, state);
          return store.get(audioOutputStateAtom)!;
        });
      saves.set(store, operation);
      void operation
        .finally(() => {
          if (saves.get(store) === operation) saves.delete(store);
        })
        .catch(() => {});
      return operation;
    },
    [store],
  );
  return { setAudioOutput };
}
