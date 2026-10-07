import { afterEach, beforeEach, describe, it, expect, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
  act,
} from "@testing-library/react";
import { Provider, createStore } from "jotai";
import type { PropsWithChildren } from "react";

// TidalImage resolves the photo through invoke("get_image_bytes"), and useAuth
// touches invoke on logout — stub the Tauri bridge so neither hits a real backend.
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn((cmd: string) =>
    cmd === "get_image_bytes"
      ? Promise.resolve(new ArrayBuffer(8))
      : Promise.resolve(undefined),
  ),
}));

import UserMenu from "./UserMenu";
import { ToastProvider } from "../contexts/ToastContext";
import { userNameAtom, currentUserAvatarAtom } from "../atoms/auth";
import { currentViewAtom } from "../atoms/navigation";
import { exclusiveDeviceAtom, exclusiveModeAtom } from "../atoms/playback";
import { invoke } from "@tauri-apps/api/core";
import {
  acceptAudioOutputAtom,
  configuredAudioOutputAtom,
} from "../atoms/audioOutput";

function renderMenu(avatar: string | null, exclusiveDevice?: string) {
  const store = createStore();
  store.set(userNameAtom, "Alice");
  store.set(currentUserAvatarAtom, avatar);
  store.set(exclusiveModeAtom, exclusiveDevice !== undefined);
  store.set(exclusiveDeviceAtom, exclusiveDevice ?? "");
  const configured = store.get(configuredAudioOutputAtom);
  store.set(acceptAudioOutputAtom, {
    configured,
    active: configured,
    pending: false,
    playbackGeneration: 1,
    revision: 0,
  });
  const wrapper = ({ children }: PropsWithChildren) => (
    <Provider store={store}>
      <ToastProvider>{children}</ToastProvider>
    </Provider>
  );
  const utils = render(<UserMenu />, { wrapper });
  // Open the dropdown via the round account trigger.
  fireEvent.click(screen.getByTitle("Account"));
  return { store, ...utils };
}

describe("UserMenu avatar + profile navigation", () => {
  afterEach(cleanup);

  it("renders the avatar photo when the atom is set", async () => {
    const { container } = renderMenu("https://img/avatar.jpg");
    // The header row name is always present.
    expect(screen.getByText("Alice")).not.toBeNull();
    // TidalImage resolves the blob asynchronously, then mounts an <img>
    // (alt="" keeps it out of the ARIA img role, so query the element directly).
    await waitFor(() => {
      expect(container.querySelectorAll("img").length).toBeGreaterThan(0);
    });
  });

  it("renders the person-icon fallback (no <img>) when the avatar is null", () => {
    const { container } = renderMenu(null);
    expect(screen.getByText("Alice")).not.toBeNull();
    expect(container.querySelector("img")).toBeNull();
  });

  it("navigates to the profile when the name row is clicked", () => {
    const { store } = renderMenu(null);
    fireEvent.click(screen.getByText("Alice"));
    expect(store.get(currentViewAtom)).toMatchObject({ type: "profile" });
  });

  it("no longer shows a separate Profile menu item", () => {
    renderMenu(null);
    expect(screen.queryByRole("button", { name: "Profile" })).toBeNull();
  });
});

describe("audio device discovery", () => {
  beforeEach(() => {
    localStorage.clear();
    vi.mocked(invoke).mockReset();
    vi.mocked(invoke).mockResolvedValue(undefined);
  });
  afterEach(cleanup);

  it("preserves the selected output when discovery returns other devices", async () => {
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "list_audio_devices")
        return [{ id: "hw:2", name: "Other DAC" }];
      if (command === "set_audio_output")
        return {
          configured: (args as { config: unknown }).config,
          active: null,
          playbackGeneration: 1,
          revision: 0,
          pending: false,
        };
      return undefined;
    });
    const { store } = renderMenu(null, "hw:1");
    await screen.findByRole("button", { name: "Refresh devices" });
    expect(invoke).toHaveBeenCalledWith("list_audio_devices", {
      forceRefresh: false,
    });
    expect(store.get(exclusiveDeviceAtom)).toBe("hw:1");
    expect(
      vi
        .mocked(invoke)
        .mock.calls.some(([command]) => command === "set_exclusive_device"),
    ).toBe(false);
    fireEvent.click(screen.getByRole("button", { name: "Select device" }));
    fireEvent.click(screen.getByRole("button", { name: "Other DAC" }));
    expect(store.get(exclusiveDeviceAtom)).toBe("hw:1");
    await waitFor(() => expect(store.get(exclusiveDeviceAtom)).toBe("hw:2"));
    expect(invoke).toHaveBeenCalledWith("set_audio_output", {
      config: expect.objectContaining({ device: "hw:2" }),
    });
  });

  it("shows empty results and retries discovery explicitly after a failure", async () => {
    vi.mocked(invoke).mockRejectedValueOnce(new Error("device scan failed"));
    const { store } = renderMenu(null, "hw:1");
    expect(await screen.findByRole("alert")).toHaveProperty(
      "textContent",
      "Unable to load audio devices. Try refreshing.",
    );
    vi.mocked(invoke).mockResolvedValueOnce([]);
    fireEvent.click(screen.getByRole("button", { name: "Refresh devices" }));
    await screen.findByText("No audio devices found.");
    expect(invoke).toHaveBeenLastCalledWith("list_audio_devices", {
      forceRefresh: true,
    });
    expect(store.get(exclusiveDeviceAtom)).toBe("hw:1");
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("ignores an old scan after closing and reopening the account menu", async () => {
    let finishOld!: (devices: Array<{ id: string; name: string }>) => void;
    vi.mocked(invoke).mockReturnValueOnce(
      new Promise((resolve) => {
        finishOld = resolve;
      }),
    );
    renderMenu(null, "hw:2");
    fireEvent.click(screen.getByTitle("Account"));
    vi.mocked(invoke).mockResolvedValueOnce([
      { id: "hw:2", name: "Current DAC" },
    ]);
    fireEvent.click(screen.getByTitle("Account"));
    await screen.findByRole("button", { name: "Current DAC" });
    await act(async () => {
      finishOld([{ id: "hw:2", name: "Outdated DAC" }]);
    });
    expect(screen.getByRole("button", { name: "Current DAC" })).toBeTruthy();
    expect(screen.queryByText("Outdated DAC")).toBeNull();
  });
});
