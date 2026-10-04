import type { SignalPath } from "../../atoms/playback";
import type { StreamInfo, Track } from "../../types";

export interface SignalPathViewProps {
  sp: SignalPath | null;
  streamInfo: StreamInfo | null;
  currentTrack: Track | null;
  onClose: () => void;
}

const EPS = 1e-3;

/** ALSA naming → canonical GStreamer naming. Pure mapping table. */
const ALSA_TO_GSTREAMER: Record<string, string> = {
  S16_LE: "S16LE",
  S24_LE: "S24_32LE",
  S24_3LE: "S24LE",
  S32_LE: "S32LE",
  FLOAT_LE: "F32LE",
  S16_BE: "S16BE",
  S24_BE: "S24_32BE",
  S24_3BE: "S24BE",
  S32_BE: "S32BE",
  FLOAT_BE: "F32BE",
};

/** Convert any naming convention (ALSA / GStreamer / pactl) to canonical GStreamer form. */
function toGstreamerFormat(s: string): string {
  // Uppercase and convert pactl's dashes to underscores.
  const upper = s.toUpperCase().replace(/-/g, "_");
  // ALSA → GStreamer.
  if (ALSA_TO_GSTREAMER[upper]) return ALSA_TO_GSTREAMER[upper];
  // pactl float aliases.
  if (upper === "FLOAT" || upper === "FLOAT32" || upper === "FLOAT32LE")
    return "F32LE";
  if (upper === "FLOAT64" || upper === "FLOAT64LE") return "F64LE";
  // Already canonical (or unknown — leave as-is).
  return upper;
}

/**
 * Render a PCM format for the UI. Canonicalizes to GStreamer naming so
 * S24_32LE (4-byte container) and S24LE (3-byte packed) stay visibly
 * distinct regardless of whether the source string came from /proc/asound
 * (ALSA), GStreamer pad caps, or pactl.
 */
export function displayFormat(s: string | null | undefined): string {
  if (!s) return "—";
  return toGstreamerFormat(s);
}

/**
 * Compare two PCM format strings for semantic equivalence across naming
 * conventions. ALSA "S24_LE" matches GStreamer "S24_32LE"; pactl "s32le"
 * matches ALSA "S32_LE"; etc.
 */
export function formatsEquivalent(
  a: string | null | undefined,
  b: string | null | undefined,
): boolean {
  if (!a || !b) return false;
  return toGstreamerFormat(a) === toGstreamerFormat(b);
}

export function formatRate(hz: number | null | undefined): string | null {
  if (!hz) return null;
  return hz >= 1000
    ? `${(hz / 1000).toFixed(hz % 1000 === 0 ? 0 : 1)} kHz`
    : `${hz} Hz`;
}

export function gainFactorToDb(factor: number): string {
  if (Math.abs(factor - 1.0) < EPS) return "0.0 dB";
  if (factor <= 0) return "−∞ dB";
  const db = 20 * Math.log10(factor);
  return `${db >= 0 ? "+" : ""}${db.toFixed(1)} dB`;
}

/**
 * Recover the in-app slider position (0-100) from the amplitude factor the
 * backend reports. The backend applies a cubic taper (slider^3 → amplitude)
 * in `slider_to_amplitude`, so `cbrt(amplitude)` returns the slider position.
 * Use this for display so the panel matches what the user sees on the
 * volume widget rather than the lower amplitude-percent.
 */
export function amplitudeToSliderPercent(amplitude: number): number {
  if (amplitude <= 0) return 0;
  return Math.round(Math.cbrt(amplitude) * 100);
}

/**
 * Friendly DAC name for compact display. Strips the ALSA driver prefix
 * ("USB-Audio - ", "HDA-Intel - ") and the trailing bus location
 * (" at usb-0000:..."). Falls back to outputDevice path. Returns null when
 * nothing usable is available.
 *
 * Example transformations:
 *   "USB-Audio - iFi (by AMR) HD USB Audio at usb-0000:00:14.0-3, high speed"
 *     → "iFi (by AMR) HD USB Audio"
 *   "HDA-Intel - HDA Intel PCH" → "HDA Intel PCH"
 */
export function dacDisplayName(sp: SignalPath | null): string | null {
  const cardName = sp?.dac?.cardName;
  if (cardName) {
    let s = cardName;
    const dashIdx = s.indexOf(" - ");
    if (dashIdx > 0) s = s.slice(dashIdx + 3);
    const atIdx = s.indexOf(" at ");
    if (atIdx > 0) s = s.slice(0, atIdx);
    const trimmed = s.trim();
    if (trimmed.length > 0) return trimmed;
  }
  return sp?.outputDevice ?? null;
}

/** Keep valid bits separate from container width; floats are not integers. */
function pcmFormat(format: string | null | undefined) {
  if (!format) return null;
  const name = toGstreamerFormat(format);
  const match = /^(S16|S24_32|S24|S32|F32|F64)(LE|BE)$/.exec(name);
  if (!match) return null;
  const kind = match[1].startsWith("F") ? "float" : "integer";
  const depth = Number(match[1].slice(1).split("_")[0]);
  return { name, kind, depth, width: match[1] === "S24_32" ? 32 : depth };
}

type Preservation = "preserved" | "modified" | "unknown";
const measured = (value: number | null | undefined): value is number =>
  typeof value === "number" && Number.isInteger(value) && value > 0;

export function classifyConversion(
  fromFmt: string | null | undefined,
  toFmt: string | null | undefined,
  fromRate: number | null | undefined,
  toRate: number | null | undefined,
  fromChannels: number | null | undefined,
  toChannels: number | null | undefined,
): Preservation {
  if (
    (measured(fromRate) && measured(toRate) && fromRate !== toRate) ||
    (measured(fromChannels) &&
      measured(toChannels) &&
      fromChannels !== toChannels)
  )
    return "modified";
  const from = pcmFormat(fromFmt);
  const to = pcmFormat(toFmt);
  // A confirmed reduction remains a modification even if another measurement
  // is absent. Missing evidence must never upgrade it to an unknown/pass-through.
  if (
    from?.kind === "integer" &&
    to?.kind === "integer" &&
    to.depth < from.depth
  )
    return "modified";
  if (
    !from ||
    !to ||
    !measured(fromRate) ||
    !measured(toRate) ||
    !measured(fromChannels) ||
    !measured(toChannels)
  )
    return "unknown";
  if (from.name === to.name) return "preserved";
  if (from.kind !== "integer" || to.kind !== "integer") return "unknown";
  return "preserved";
}

export function conversionState(
  ...args: Parameters<typeof classifyConversion>
): "altered" | "lossy" | "unknown" {
  const result = classifyConversion(...args);
  return result === "preserved"
    ? "altered"
    : result === "modified"
      ? "lossy"
      : "unknown";
}

export function deriveAlterations(sp: SignalPath | null) {
  const userVol = sp?.userVolume ?? 1;
  const normFactor = sp?.normGainFactor ?? 1;
  const userVolKnown =
    typeof sp?.userVolume === "number" &&
    Number.isFinite(sp.userVolume) &&
    sp.userVolume >= 0;
  const normKnown =
    typeof sp?.normGainFactor === "number" &&
    Number.isFinite(sp.normGainFactor) &&
    sp.normGainFactor >= 0;
  // Even a small non-unity gain changes PCM samples; display rounding must not
  // be used to decide whether the audio is untouched.
  const userVolAltered = userVolKnown && userVol !== 1;
  const normAltered =
    !!sp?.volumeNormalization && normKnown && normFactor !== 1;
  const gainsKnown =
    userVolKnown &&
    typeof sp?.volumeNormalization === "boolean" &&
    (!sp.volumeNormalization || normKnown);
  const isDirectAlsa = sp?.backend === "DirectAlsa";
  const conversion = classifyConversion(
    sp?.decodedFormat,
    sp?.outputFormat,
    sp?.decodedRate,
    sp?.outputRate,
    sp?.decodedChannels,
    sp?.outputChannels,
  );
  const formatChanged =
    !!sp?.decodedFormat &&
    !!sp?.outputFormat &&
    !formatsEquivalent(sp.decodedFormat, sp.outputFormat);
  const from = pcmFormat(sp?.decodedFormat);
  const to = pcmFormat(sp?.outputFormat);
  const lossyFormatChange =
    !!from &&
    !!to &&
    from.kind === "integer" &&
    to.kind === "integer" &&
    to.depth < from.depth;
  const losslessPromotion = formatChanged && conversion === "preserved";
  const mixer = !isDirectAlsa ? sp?.osMixer : null;
  const mixerConversion = mixer
    ? classifyConversion(
        sp?.outputFormat,
        mixer.sinkFormat,
        sp?.outputRate,
        mixer.sinkRate,
        sp?.outputChannels,
        mixer.sinkChannels,
      )
    : "unknown";
  const dacConversion =
    sp?.dac?.state === "Active"
      ? classifyConversion(
          mixer?.sinkFormat ?? sp.outputFormat,
          sp.dac.format,
          mixer?.sinkRate ?? sp.outputRate,
          sp.dac.rate,
          mixer?.sinkChannels ?? sp.outputChannels,
          sp.dac.channels,
        )
      : "unknown";
  const dacMatchesPipeline =
    sp?.dac?.state === "Active" &&
    formatsEquivalent(sp.dac.format, sp.outputFormat) &&
    sp.dac.rate === sp.outputRate &&
    sp.dac.channels === sp.outputChannels;
  const knownModification =
    userVolAltered ||
    normAltered ||
    conversion === "modified" ||
    mixerConversion === "modified" ||
    dacConversion === "modified" ||
    (measured(sp?.resampledFrom) &&
      measured(sp?.resampledTo) &&
      sp.resampledFrom !== sp.resampledTo) ||
    (!!mixer &&
      (mixer.sinkMuted ||
        (Number.isFinite(mixer.sinkVolume) && mixer.sinkVolume !== 1)));
  const isPristine =
    !!sp &&
    isDirectAlsa &&
    sp.exclusiveMode &&
    gainsKnown &&
    conversion === "preserved" &&
    !knownModification &&
    !!dacMatchesPipeline;
  const verdict: Preservation = isPristine
    ? "preserved"
    : knownModification
      ? "modified"
      : "unknown";
  return {
    userVol,
    normFactor,
    userVolAltered,
    normAltered,
    isDirectAlsa,
    isPristine,
    lossyFormatChange,
    losslessPromotion,
    verdict,
  };
}
