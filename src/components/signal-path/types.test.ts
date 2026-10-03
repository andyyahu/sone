import { describe, expect, it } from "vitest";
import type { SignalPath } from "../../atoms/playback";
import {
  camillaHeadline,
  channelPadNote,
  deriveAlterations,
  HQ_HEADLINE,
  HQ_VERDICT,
  replayGainReason,
  signalHeadline,
  signalVerdictWord,
  volumeReason,
} from "./types";

function path(over: Partial<SignalPath> = {}): SignalPath {
  return {
    backend: "DirectAlsa",
    decodedFormat: "S24_32LE",
    decodedRate: 96000,
    decodedChannels: 2,
    outputFormat: "S24_32LE",
    outputRate: 96000,
    outputChannels: 2,
    outputDevice: null,
    exclusiveMode: true,
    bitPerfect: true,
    volumeNormalization: false,
    userVolume: 1,
    normGainFactor: 1,
    resampledFrom: null,
    resampledTo: null,
    promotedFrom: null,
    promotedTo: null,
    formatFallbackFrom: null,
    formatFallbackTo: null,
    dac: null,
    osMixer: null,
    camillaFir: false,
    ...over,
  };
}

describe("channelPadNote", () => {
  it("describes a stereo source padded out to a fixed-channel DAC", () => {
    expect(channelPadNote(2, 8)).toBe(
      "stereo is padded with silence to 8 channels; the source pair is unchanged",
    );
  });

  it("stays quiet when the channel count already matches", () => {
    expect(channelPadNote(2, 2)).toBeNull();
  });

  it("stays quiet when either side is unknown", () => {
    expect(channelPadNote(null, 8)).toBeNull();
    expect(channelPadNote(2, null)).toBeNull();
  });

  it("does not claim a downmix or a non-stereo layout left the source untouched", () => {
    expect(channelPadNote(2, 1)).toBeNull();
    expect(channelPadNote(6, 8)).toBeNull();
  });
});

describe("deriveAlterations camilla FIR", () => {
  it("stays pristine on an untouched exclusive path", () => {
    expect(deriveAlterations(path()).isPristine).toBe(true);
  });

  it("is modified while CamillaDSP is convolving", () => {
    expect(deriveAlterations(path({ camillaFir: true })).isPristine).toBe(
      false,
    );
  });

  it("is not pristine when HQPlayer owns the device", () => {
    expect(
      deriveAlterations(path({ backend: "HQPlayer", exclusiveMode: false }))
        .isPristine,
    ).toBe(false);
  });
});

describe("signal path wording", () => {
  it("calls an HQPlayer handoff a handoff even when gain factors are stored", () => {
    const sp = path({
      backend: "HQPlayer",
      exclusiveMode: false,
      bitPerfect: false,
      dac: null,
      outputFormat: "S32LE",
      userVolume: 0.125,
      volumeNormalization: true,
      normGainFactor: 0.5,
    });
    expect(signalHeadline(sp, "FLAC 24/96")).toBe(HQ_HEADLINE);
    expect(signalVerdictWord(sp, false)).toBe(HQ_VERDICT);
  });

  it("names CamillaDSP and a non-unity quantize gain together", () => {
    const headline = camillaHeadline(0.125, 0.5, true, true);
    expect(headline).toContain("CamillaDSP");
    expect(headline).toContain("50%");
    expect(headline).toContain("ReplayGain -6.0 dB");
    expect(
      signalHeadline(
        path({
          camillaFir: true,
          userVolume: 0.125,
          volumeNormalization: true,
          normGainFactor: 0.5,
        }),
        "",
      ),
    ).toBe(headline);
  });

  it("keeps the byte-path gain wording when CamillaDSP is off", () => {
    expect(volumeReason(false)).toContain("writer thread");
    expect(replayGainReason(false)).toContain("before output");
    expect(volumeReason(true)).toContain("quantize after CamillaDSP");
    expect(replayGainReason(true)).toContain("quantize after CamillaDSP");
  });
});
