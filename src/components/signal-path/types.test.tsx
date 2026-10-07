import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import type { SignalPath } from "../../atoms/playback";
import { classifyConversion, deriveAlterations } from "./types";
import FlowDiagramBody from "./FlowDiagramBody";

function path(overrides: Partial<SignalPath> = {}): SignalPath {
  return {
    backend: "DirectAlsa",
    decodedFormat: "S24LE",
    decodedRate: 96000,
    decodedChannels: 2,
    outputFormat: "S32LE",
    outputRate: 96000,
    outputChannels: 2,
    outputDevice: "hw:1",
    exclusiveMode: true,
    bitPerfect: true,
    volumeNormalization: false,
    userVolume: 1,
    normGainFactor: 1,
    resampledFrom: null,
    resampledTo: null,
    promotedFrom: "S24LE",
    promotedTo: "S32LE",
    formatFallbackFrom: null,
    formatFallbackTo: null,
    osMixer: null,
    dac: {
      cardIndex: 1,
      cardName: "Test DAC",
      pcmDevice: "hw:1",
      format: "S32_LE",
      rate: 96000,
      channels: 2,
      periodSize: 1024,
      bufferSize: 4096,
      state: "Active",
    },
    ...overrides,
  };
}

afterEach(cleanup);

describe("measured sample preservation", () => {
  it.each([
    ["S24LE", "S24_32LE", "preserved"],
    ["S24_32LE", "S24_3LE", "preserved"],
    ["S16LE", "S32_LE", "preserved"],
    ["S32LE", "S24LE", "modified"],
    ["S24LE", "S16LE", "modified"],
    ["F32LE", "FLOAT_LE", "preserved"],
    ["S32LE", "F32LE", "unknown"],
    ["F32LE", "F64LE", "unknown"],
    ["mystery", "mystery", "unknown"],
  ])("classifies %s → %s as %s", (from, to, expected) => {
    expect(classifyConversion(from, to, 96000, 96000, 2, 2)).toBe(expected);
  });

  it("does not infer preservation from missing caps, rates or channels", () => {
    expect(deriveAlterations(null).verdict).toBe("unknown");
    for (const field of [
      "decodedFormat",
      "outputFormat",
      "decodedRate",
      "outputRate",
      "decodedChannels",
      "outputChannels",
    ] as const) {
      expect(deriveAlterations(path({ [field]: null })).verdict).toBe(
        "unknown",
      );
    }
    expect(classifyConversion("S24LE", "S16LE", null, null, null, null)).toBe(
      "modified",
    );
    expect(classifyConversion("S24LE", "S24LE", 96000, 48000, 2, 2)).toBe(
      "modified",
    );
    expect(classifyConversion("S24LE", "S24LE", 96000, 96000, 2, 1)).toBe(
      "modified",
    );
  });

  it("requires measured DAC output and unity gain for a pristine verdict", () => {
    expect(deriveAlterations(path())).toMatchObject({
      verdict: "preserved",
      isPristine: true,
      losslessPromotion: true,
    });
    expect(deriveAlterations(path({ dac: null })).verdict).toBe("unknown");
    expect(
      deriveAlterations(path({ dac: { ...path().dac!, state: "Closed" } }))
        .verdict,
    ).toBe("unknown");
    expect(deriveAlterations(path({ userVolume: 0.9999 })).verdict).toBe(
      "modified",
    );
    expect(
      deriveAlterations(
        path({ volumeNormalization: true, normGainFactor: 0.5 }),
      ).verdict,
    ).toBe("modified");
    expect(deriveAlterations(path({ userVolume: NaN })).verdict).toBe(
      "unknown",
    );
  });

  it("includes confirmed DAC and OS mixer changes in the verdict", () => {
    expect(
      deriveAlterations(path({ dac: { ...path().dac!, rate: 48000 } })).verdict,
    ).toBe("modified");
    expect(
      deriveAlterations(path({ dac: { ...path().dac!, channels: 1 } })).verdict,
    ).toBe("modified");
    expect(
      deriveAlterations(
        path({
          backend: "Normal",
          exclusiveMode: false,
          osMixer: {
            server: "PipeWire",
            defaultSinkName: "test",
            sinkFormat: "S32LE",
            sinkRate: 48000,
            sinkChannels: 2,
            sinkVolume: 1,
            sinkVolumePercent: 100,
            sinkMuted: false,
          },
        }),
      ).verdict,
    ).toBe("modified");
  });
});

describe("signal path explanations", () => {
  it("labels missing measurements as unknown even when bit-perfect mode is enabled", () => {
    render(
      <FlowDiagramBody
        sp={path({ decodedChannels: null })}
        streamInfo={null}
        currentTrack={null}
      />,
    );
    expect(
      screen.getByText("UNKNOWN — INSUFFICIENT MEASUREMENTS"),
    ).toBeTruthy();
    expect(screen.getByText("BIT-PERFECT MODE")).toBeTruthy();
    expect(screen.queryByText("PROMOTED")).toBeNull();
  });

  it("does not present narrowing as a lossless promotion from stale tracker flags", () => {
    render(
      <FlowDiagramBody
        sp={path({
          outputFormat: "S16LE",
          promotedTo: "S16LE",
          dac: { ...path().dac!, format: "S16_LE" },
        })}
        streamInfo={null}
        currentTrack={null}
      />,
    );
    expect(screen.getByText("FORMAT CONVERSION")).toBeTruthy();
    expect(screen.queryByText("PROMOTED")).toBeNull();
    expect(
      screen.queryByText("Lossless zero-pad — sample values preserved"),
    ).toBeNull();
    expect(screen.getByText(/NOT PRISTINE —/)).toBeTruthy();
  });

  it("identifies confirmed integer widening as preserved", () => {
    render(
      <FlowDiagramBody sp={path()} streamInfo={null} currentTrack={null} />,
    );
    expect(
      screen.getByText("PRISTINE · BIT-TRANSPARENT — LOSSLESS PROMOTION"),
    ).toBeTruthy();
    expect(screen.getByText("PROMOTED")).toBeTruthy();
  });
});

describe("external and DSP signal paths", () => {
  it("always identifies CamillaDSP samples as modified", () => {
    expect(deriveAlterations(path({ camillaFir: true }))).toMatchObject({
      verdict: "modified",
      isPristine: false,
    });
    render(
      <FlowDiagramBody
        sp={path({ camillaFir: true })}
        streamInfo={null}
        currentTrack={null}
      />,
    );
    expect(screen.getByText("CAMILLA DSP")).toBeTruthy();
    expect(screen.queryByText("BIT-PERFECT MODE")).toBeNull();
    expect(screen.queryByText(/PRISTINE · BIT-TRANSPARENT/)).toBeNull();
  });
  it("does not infer HQPlayer downstream preservation from local PCM or stale DAC measurements", () => {
    const sp = path({ backend: "HQPlayer" });
    expect(deriveAlterations(sp)).toMatchObject({
      verdict: "unknown",
      isPristine: false,
    });
    render(<FlowDiagramBody sp={sp} streamInfo={null} currentTrack={null} />);
    expect(screen.getByText("HQPlayer Desktop")).toBeTruthy();
    expect(screen.getByText(/DAC measurements are unavailable/)).toBeTruthy();
    expect(screen.queryByText("Test DAC")).toBeNull();
    expect(screen.queryByText("BIT-PERFECT MODE")).toBeNull();
    expect(screen.queryByText("PRISTINE")).toBeNull();
  });
});
