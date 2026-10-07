import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { Provider, createStore } from "jotai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import AudioOutputSettings from "./AudioOutputSettings";
import {
  acceptAudioOutputAtom,
  configuredAudioOutputAtom,
  type AudioOutputConfig,
  type AudioOutputState,
} from "../../atoms/audioOutput";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
const native: AudioOutputConfig = {
  route: "native",
  exclusiveMode: false,
  bitPerfect: false,
  device: "hw:2",
  camillaConfig: null,
  hqplayerHost: "127.0.0.1",
  hqplayerPort: 4321,
};
function setup(configured = native, active: AudioOutputConfig | null = native) {
  const store = createStore();
  store.set(acceptAudioOutputAtom, {
    configured,
    active,
    playbackGeneration: 1,
    revision: 0,
    pending: configured !== active,
  });
  render(
    <Provider store={store}>
      <AudioOutputSettings />
    </Provider>,
  );
  return store;
}
beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    if (command === "list_audio_devices") return [];
    if (command === "pick_camilla_config") return null;
    return {
      configured: (args as { config: AudioOutputConfig }).config,
      active: native,
      playbackGeneration: 1,
      revision: 0,
      pending: true,
    };
  });
});
afterEach(cleanup);

describe("audio output settings", () => {
  it("shows configured and active routes separately until the next playback", async () => {
    const store = setup();
    fireEvent.change(screen.getByRole("combobox", { name: "Audio output" }), {
      target: { value: "hqplayer" },
    });
    await waitFor(() =>
      expect(store.get(configuredAudioOutputAtom).route).toBe("hqplayer"),
    );
    expect(screen.getByText("Active: Native")).toBeTruthy();
    expect(screen.getByText("Applies on next playback")).toBeTruthy();
    expect(
      screen.getByText(/Manage volume and processing in HQPlayer/),
    ).toBeTruthy();
  });
  it("does not claim a changed preference until the save succeeds", async () => {
    let finish!: (state: AudioOutputState) => void;
    vi.mocked(invoke).mockImplementation(
      () =>
        new Promise((resolve) => {
          finish = resolve;
        }),
    );
    const store = setup();
    fireEvent.click(screen.getByRole("button", { name: "Exclusive output" }));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "set_audio_output",
        expect.anything(),
      ),
    );
    expect(store.get(configuredAudioOutputAtom).exclusiveMode).toBe(false);
    expect(
      screen.getByRole("button", { name: "Exclusive output" }),
    ).toHaveProperty("disabled", true);
    await act(async () =>
      finish({
        configured: { ...native, exclusiveMode: true },
        active: native,
        playbackGeneration: 1,
        revision: 0,
        pending: true,
      }),
    );
    expect(store.get(configuredAudioOutputAtom).exclusiveMode).toBe(true);
  });
  it("keeps native output selected when the configuration picker is cancelled", async () => {
    const store = setup();
    fireEvent.change(screen.getByRole("combobox", { name: "Audio output" }), {
      target: { value: "camilla" },
    });
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("pick_camilla_config"),
    );
    expect(store.get(configuredAudioOutputAtom).route).toBe("native");
    expect(
      vi
        .mocked(invoke)
        .mock.calls.some(([command]) => command === "set_audio_output"),
    ).toBe(false);
  });
  it("reports invalid Camilla configuration without changing the selected route", async () => {
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "pick_camilla_config") return "/tmp/invalid.yml";
      throw new Error("Invalid filter configuration");
    });
    const store = setup();
    fireEvent.change(screen.getByRole("combobox", { name: "Audio output" }), {
      target: { value: "camilla" },
    });
    expect(await screen.findByRole("alert")).toHaveProperty(
      "textContent",
      "Invalid filter configuration",
    );
    expect(store.get(configuredAudioOutputAtom)).toEqual(native);
  });
  it("activates Camilla with a validated file while preserving native preferences", async () => {
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "pick_camilla_config") return "/tmp/room.yml";
      if (command === "list_audio_devices") return [];
      return {
        configured: (args as { config: AudioOutputConfig }).config,
        active: native,
        playbackGeneration: 1,
        revision: 0,
        pending: true,
      };
    });
    const store = setup();
    fireEvent.change(screen.getByRole("combobox", { name: "Audio output" }), {
      target: { value: "camilla" },
    });
    await screen.findByText("room.yml");
    expect(store.get(configuredAudioOutputAtom)).toMatchObject({
      route: "camilla",
      exclusiveMode: false,
      bitPerfect: false,
      device: "hw:2",
    });
  });
  it("rejects invalid HQPlayer port input before IPC", () => {
    setup({ ...native, route: "hqplayer" });
    fireEvent.change(screen.getByLabelText("Control port (localhost)"), {
      target: { value: "65536" },
    });
    expect(screen.getByRole("button", { name: "Apply" })).toHaveProperty(
      "disabled",
      true,
    );
    expect(invoke).not.toHaveBeenCalled();
  });
});

it("lets a failed HQPlayer connection be retried on a custom local port without changing active output", async () => {
  vi.mocked(invoke).mockRejectedValueOnce(new Error("HQPlayer unavailable"));
  const store = setup();
  fireEvent.change(screen.getByRole("combobox", { name: "Audio output" }), {
    target: { value: "hqplayer" },
  });
  await screen.findByText("HQPlayer unavailable");
  expect(store.get(configuredAudioOutputAtom).route).toBe("native");
  fireEvent.change(screen.getByLabelText("Control port (localhost)"), {
    target: { value: "4322" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Use HQPlayer" }));
  await waitFor(() =>
    expect(store.get(configuredAudioOutputAtom)).toMatchObject({
      route: "hqplayer",
      hqplayerPort: 4322,
      hqplayerHost: "127.0.0.1",
    }),
  );
  expect(screen.getByText("Active: Native")).toBeTruthy();
});

it("shows an unsupported stored HQPlayer endpoint and permits explicit localhost correction on the same port", async () => {
  const store = setup(
    { ...native, route: "hqplayer", hqplayerHost: "192.168.1.20" },
    null,
  );
  expect(screen.getByRole("alert").textContent).toContain("192.168.1.20:4321");
  expect(store.get(configuredAudioOutputAtom).hqplayerHost).toBe(
    "192.168.1.20",
  );
  const correct = screen.getByRole("button", { name: "Use localhost" });
  expect(correct).toHaveProperty("disabled", false);
  fireEvent.click(correct);
  await waitFor(() =>
    expect(store.get(configuredAudioOutputAtom)).toMatchObject({
      hqplayerHost: "127.0.0.1",
      hqplayerPort: 4321,
    }),
  );
});

it("disables configuration controls while backend preferences are loading", () => {
  const store = createStore();
  render(
    <Provider store={store}>
      <AudioOutputSettings />
    </Provider>,
  );
  expect(screen.getByText("Loading audio output…")).toBeTruthy();
  expect(screen.getByRole("combobox", { name: "Audio output" })).toHaveProperty(
    "disabled",
    true,
  );
  expect(
    screen.getByRole("button", { name: "Exclusive output" }),
  ).toHaveProperty("disabled", true);
  expect(invoke).not.toHaveBeenCalled();
});
