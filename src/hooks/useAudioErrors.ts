import { useEffect } from "react";
import { useStore } from "jotai";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { isPlayingAtom, streamInfoAtom } from "../atoms/playback";
import { useToast } from "../contexts/ToastContext";
import { BIT_PERFECT_UNSUPPORTED_MESSAGE } from "../lib/errorUtils";

/** Async pipeline errors stop playback without treating a DAC failure as a bad track. */
export function useAudioErrors() {
  const store = useStore();
  const { showToast } = useToast();
  useEffect(() => {
    let active = true;
    const unlisten = listen<{ kind: string; message?: string }>(
      "audio-error",
      ({ payload: { kind, message } }) => {
        if (!active) return;
        store.set(isPlayingAtom, false);
        if (
          kind === "device_disconnected" ||
          kind === "playback_error" ||
          kind === "bit_perfect_unsupported"
        ) {
          store.set(streamInfoAtom, null);
        }
        if (kind === "bit_perfect_unsupported") {
          invoke("stop_track").catch(() => {});
          showToast(BIT_PERFECT_UNSUPPORTED_MESSAGE, "error");
        } else if (kind === "device_busy") {
          showToast(
            "Audio device is busy — close other apps using it",
            "error",
          );
        } else {
          const display =
            message && message.length > 80
              ? message.slice(0, 80) + "…"
              : message || "Playback error";
          showToast(display, "error");
        }
      },
    );
    return () => {
      active = false;
      void unlisten.then((fn) => fn());
    };
  }, [store, showToast]);
}
