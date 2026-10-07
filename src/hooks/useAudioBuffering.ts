import { useEffect } from "react";
import { useStore } from "jotai";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { audioBufferingAtom, audioOutputStateAtom } from "../atoms/audioOutput";
import {
  currentTrackAtom,
  isPlayingAtom,
  userPausedAtom,
} from "../atoms/playback";
import { markPlaybackLoading } from "../lib/playbackPosition";

/** HQPlayer reconnects must not count unheard time as playback or scrobbling. */
export function useAudioBuffering() {
  const store = useStore();
  useEffect(() => {
    let disposed = false;
    let bufferingTrack = store.get(currentTrackAtom);
    const unlisten = listen<{
      buffering: boolean;
      trackId: number;
      playbackGeneration: number;
    }>("audio-buffering", ({ payload }) => {
      const track = store.get(currentTrackAtom);
      const output = store.get(audioOutputStateAtom);
      if (
        disposed ||
        !track ||
        track.id !== payload.trackId ||
        output?.active?.route !== "hqplayer" ||
        output.playbackGeneration !== payload.playbackGeneration
      )
        return;
      const buffering = store.get(audioBufferingAtom);
      if (payload.buffering) {
        if (buffering && bufferingTrack === track) return;
        bufferingTrack = track;
        const wasPlaying = store.get(isPlayingAtom);
        store.set(audioBufferingAtom, {
          trackId: track.id,
          playbackGeneration: payload.playbackGeneration,
          wasPlaying,
        });
        // Snapshot the live interpolated position before enabling the load gate.
        store.set(isPlayingAtom, false);
        markPlaybackLoading(true);
        if (wasPlaying) void invoke("notify_track_paused").catch(() => {});
      } else if (buffering && bufferingTrack === track) {
        store.set(audioBufferingAtom, null);
        markPlaybackLoading(false);
        if (buffering.wasPlaying && !store.get(userPausedAtom)) {
          store.set(isPlayingAtom, true);
          void invoke("notify_track_resumed").catch(() => {});
        }
      }
    });
    const clearStale = () => {
      const output = store.get(audioOutputStateAtom);
      const buffering = store.get(audioBufferingAtom);
      if (
        store.get(currentTrackAtom) !== bufferingTrack ||
        output?.active?.route !== "hqplayer" ||
        (buffering &&
          buffering.playbackGeneration !== output.playbackGeneration)
      ) {
        store.set(audioBufferingAtom, null);
      }
    };
    const unsubTrack = store.sub(currentTrackAtom, clearStale);
    const unsubOutput = store.sub(audioOutputStateAtom, clearStale);
    return () => {
      disposed = true;
      unsubTrack();
      unsubOutput();
      void unlisten.then((stop) => stop()).catch(() => {});
    };
  }, [store]);
}
