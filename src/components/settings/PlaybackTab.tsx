import { useEffect, useState } from "react";
import { useAtom, useAtomValue, useStore } from "jotai";
import { invoke } from "@tauri-apps/api/core";
import {
  autoplayAtom,
  volumeNormalizationAtom,
  gaplessAtom,
  allowExplicitAtom,
  currentTrackAtom,
  isPlayingAtom,
  queueAtom,
  manualQueueAtom,
  originalQueueAtom,
  historyAtom,
  playbackSourceAtom,
  contextSourceAtom,
} from "../../atoms/playback";
import {
  audioControlLockAtom,
  effectiveAudioOutputAtom,
} from "../../atoms/audioOutput";
import { usePlaybackActions } from "../../hooks/usePlaybackActions";
import { videoCoversAtom } from "../../atoms/ui";
import Toggle from "../Toggle";
import SettingRow from "./SettingRow";
import QualityPicker from "./QualityPicker";

export default function PlaybackTab() {
  const [autoplay, setAutoplay] = useAtom(autoplayAtom);
  const [videoCovers, setVideoCovers] = useAtom(videoCoversAtom);
  const volumeNormalization = useAtomValue(volumeNormalizationAtom);
  const { setVolumeNormalization } = usePlaybackActions();
  const [allowExplicit, setAllowExplicit] = useAtom(allowExplicitAtom);
  const [gapless, setGapless] = useAtom(gaplessAtom);
  const controlLock = useAtomValue(audioControlLockAtom);
  const output = useAtomValue(effectiveAudioOutputAtom);
  const [gaplessSupported, setGaplessSupported] = useState(false);
  const store = useStore();

  useEffect(() => {
    invoke<boolean>("get_gapless_supported")
      .then(setGaplessSupported)
      .catch(() => {});
  }, []);

  const gaplessDisabled =
    output.route !== "hqplayer" &&
    (output.route === "camilla" ||
      !gaplessSupported ||
      output.exclusiveMode ||
      output.bitPerfect);

  return (
    <div>
      <p className="text-[10.5px] font-bold tracking-[1.4px] uppercase text-th-text-faint mb-2.5">
        Audio quality
      </p>
      <QualityPicker />
      <p className="text-[11px] text-th-text-muted mt-2.5">
        Caps the quality requested from Tidal. Playback steps down automatically
        when a track isn't available at this tier.
      </p>

      <p className="text-[10.5px] font-bold tracking-[1.4px] uppercase text-th-text-faint mt-6 mb-1">
        Playback
      </p>
      <div
        className="rounded-[14px] bg-th-surface border border-th-border-subtle overflow-hidden divide-y divide-th-border-subtle"
        style={{ boxShadow: "inset 0 2px 8px rgba(0,0,0,.32)" }}
      >
        <SettingRow
          title="Autoplay"
          subtitle="Play similar tracks when the queue ends"
        >
          <button onClick={() => setAutoplay(!autoplay)}>
            <Toggle on={autoplay} />
          </button>
        </SettingRow>

        <SettingRow
          title={
            <span className="flex items-center gap-2">
              Gapless playback
              <span className="text-[10px] font-bold text-th-accent bg-th-accent/12 border border-th-accent/35 rounded-full px-2 py-px">
                Native / HQPlayer
              </span>
            </span>
          }
          subtitle="Seamless transitions between continuous tracks"
          disabled={gaplessDisabled}
          tooltip={
            !gaplessDisabled
              ? undefined
              : output.route === "camilla"
                ? "CamillaDSP uses non-gapless exclusive output"
                : !gaplessSupported
                  ? "Requires GStreamer 1.24 or newer"
                  : "Available in native shared output or HQPlayer"
          }
        >
          <button
            disabled={gaplessDisabled}
            className="disabled:cursor-not-allowed"
            onClick={async () => {
              const next = !gapless;
              setGapless(next);
              await invoke("set_gapless", { enabled: next }).catch(() => {});
            }}
          >
            <Toggle on={gapless && !gaplessDisabled} />
          </button>
        </SettingRow>

        <SettingRow
          title="Normalize volume"
          subtitle={
            controlLock === "hqplayer"
              ? "Manage volume and processing in HQPlayer"
              : controlLock
                ? "Disabled while bit-perfect output is on"
                : "Even out volume differences between tracks"
          }
          disabled={controlLock !== null}
          tooltip={
            controlLock === "hqplayer"
              ? "Controlled by HQPlayer"
              : controlLock
                ? "Disabled in bit-perfect mode"
                : undefined
          }
        >
          <button
            disabled={controlLock !== null}
            className="disabled:cursor-not-allowed"
            onClick={() => {
              if (controlLock) return;
              void setVolumeNormalization(!volumeNormalization);
            }}
          >
            <Toggle on={!controlLock && volumeNormalization} />
          </button>
        </SettingRow>

        <SettingRow
          title="Animated album covers"
          subtitle="Play motion covers in the player when available"
        >
          <button onClick={() => setVideoCovers(!videoCovers)}>
            <Toggle on={videoCovers} />
          </button>
        </SettingRow>

        <SettingRow
          title="Allow explicit content"
          subtitle="Allow playing tracks marked as explicit"
        >
          <button
            onClick={() => {
              setAllowExplicit(!allowExplicit);
              invoke("stop_track").catch(() => {});
              store.set(currentTrackAtom, null);
              store.set(isPlayingAtom, false);
              store.set(queueAtom, []);
              store.set(manualQueueAtom, []);
              store.set(originalQueueAtom, null);
              store.set(historyAtom, []);
              store.set(playbackSourceAtom, null);
              store.set(contextSourceAtom, null);
            }}
          >
            <Toggle on={allowExplicit} />
          </button>
        </SettingRow>
      </div>
    </div>
  );
}
