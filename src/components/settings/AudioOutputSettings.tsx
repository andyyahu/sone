import { useCallback, useEffect, useRef, useState } from "react";
import { useAtomValue } from "jotai";
import { invoke } from "@tauri-apps/api/core";
import { ChevronDown, RefreshCw } from "lucide-react";
import {
  audioOutputStateAtom,
  configuredAudioOutputAtom,
  type AudioOutputConfig,
} from "../../atoms/audioOutput";
import { useAudioOutputActions } from "../../hooks/useAudioOutput";
import Toggle from "../Toggle";

const labels = {
  native: "Native",
  camilla: "CamillaDSP",
  hqplayer: "HQPlayer",
};
function errorMessage(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  if (
    error &&
    typeof error === "object" &&
    "message" in error &&
    typeof error.message === "string"
  )
    return error.message;
  return "Unable to save audio output. Your previous settings are unchanged.";
}

export default function AudioOutputSettings() {
  const config = useAtomValue(configuredAudioOutputAtom);
  const state = useAtomValue(audioOutputStateAtom);
  const ready = state !== null;
  const unsupportedHqHost = !["127.0.0.1", "::1", "localhost"].includes(
    config.hqplayerHost.trim().toLowerCase(),
  );
  const { setAudioOutput } = useAudioOutputActions();
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [devices, setDevices] = useState<Array<{ id: string; name: string }>>(
    [],
  );
  const [devicesLoading, setDevicesLoading] = useState(false);
  const [devicesError, setDevicesError] = useState(false);
  const [deviceDropdownOpen, setDeviceDropdownOpen] = useState(false);
  const [port, setPort] = useState(String(config.hqplayerPort));
  const [hqSetupOpen, setHqSetupOpen] = useState(false);
  const deviceRequest = useRef(0);
  const invalidateDevices = useCallback(() => {
    deviceRequest.current++;
  }, []);
  const busy = useRef(false);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      invalidateDevices();
    };
  }, [invalidateDevices]);
  useEffect(() => setPort(String(config.hqplayerPort)), [config.hqplayerPort]);
  const save = async (patch: Partial<AudioOutputConfig>) => {
    if (busy.current) return;
    busy.current = true;
    setSaving(true);
    setError(null);
    try {
      await setAudioOutput(patch);
      if (mounted.current) setHqSetupOpen(false);
    } catch (err) {
      if (mounted.current) {
        setError(errorMessage(err));
        if (patch.route === "hqplayer") setHqSetupOpen(true);
      }
    } finally {
      busy.current = false;
      if (mounted.current) setSaving(false);
    }
  };
  const loadDevices = useCallback((forceRefresh = false) => {
    const request = ++deviceRequest.current;
    setDevicesLoading(true);
    setDevicesError(false);
    invoke<Array<{ id: string; name: string }>>("list_audio_devices", {
      forceRefresh,
    })
      .then((result) => {
        if (request === deviceRequest.current) setDevices(result);
      })
      .catch(() => {
        if (request === deviceRequest.current) setDevicesError(true);
      })
      .finally(() => {
        if (request === deviceRequest.current) setDevicesLoading(false);
      });
  }, []);
  const showDevices =
    config.route === "camilla" ||
    (config.route === "native" && config.exclusiveMode);
  useEffect(() => {
    if (showDevices) loadDevices();
    return () => {
      invalidateDevices();
    };
  }, [showDevices, loadDevices, invalidateDevices]);
  const chooseCamilla = async () => {
    if (busy.current) return;
    busy.current = true;
    setSaving(true);
    setError(null);
    try {
      const path = await invoke<string | null>("pick_camilla_config");
      if (path) await setAudioOutput({ route: "camilla", camillaConfig: path });
    } catch (err) {
      if (mounted.current) setError(errorMessage(err));
    } finally {
      busy.current = false;
      if (mounted.current) setSaving(false);
    }
  };
  const row =
    "w-full flex items-center justify-between gap-2 py-2 text-[12px] text-th-text-secondary disabled:opacity-50";
  const input =
    "w-full rounded-md bg-th-inset border border-th-border-subtle px-2.5 py-1.5 text-[12px] text-th-text-secondary";
  return (
    <section
      aria-label="Audio output"
      className="px-4 py-3 border-b border-th-border-subtle"
    >
      <label
        className="text-[12px] text-th-text-primary font-medium"
        htmlFor="audio-output-route"
      >
        Audio output
      </label>
      <select
        id="audio-output-route"
        className={`${input} mt-2`}
        value={config.route}
        disabled={saving || !ready}
        onChange={(event) => {
          const route = event.target.value as AudioOutputConfig["route"];
          if (route === "camilla" && !config.camillaConfig)
            void chooseCamilla();
          else void save({ route });
        }}
      >
        <option value="native">Native</option>
        <option value="camilla">CamillaDSP (experimental)</option>
        <option value="hqplayer">HQPlayer (experimental)</option>
      </select>
      <p className="text-[11px] text-th-text-muted mt-2">
        Active: {state?.active ? labels[state.active.route] : "Idle"}
      </p>
      {state?.pending && (
        <p role="status" className="text-[11px] text-th-accent mt-1">
          Applies on next playback
        </p>
      )}
      {!ready && (
        <p role="status" className="text-[11px] text-th-text-muted mt-1">
          Loading audio output…
        </p>
      )}
      {saving && (
        <p role="status" className="text-[11px] text-th-text-muted mt-1">
          Saving audio output…
        </p>
      )}
      {error && (
        <p role="alert" className="text-[12px] text-red-400 mt-2 break-words">
          {error}
        </p>
      )}
      {config.route === "native" && (
        <>
          <button
            className={row}
            disabled={saving || !ready}
            onClick={() =>
              void save({
                exclusiveMode: !config.exclusiveMode,
                ...(!config.exclusiveMode ? {} : { bitPerfect: false }),
              })
            }
          >
            <span>Exclusive output</span>
            <Toggle on={config.exclusiveMode} />
          </button>
          {config.exclusiveMode && (
            <button
              className={row}
              disabled={saving || !ready}
              onClick={() => void save({ bitPerfect: !config.bitPerfect })}
            >
              <span>Bit-perfect</span>
              <Toggle on={config.bitPerfect} />
            </button>
          )}
        </>
      )}
      {config.route === "camilla" && (
        <div className="mt-2 text-[11px] text-th-text-muted">
          <p>DSP through exclusive ALSA at the source rate. Not bit-perfect.</p>
          <p
            title={config.camillaConfig ?? undefined}
            className="truncate mt-2"
          >
            {config.camillaConfig?.split("/").pop() ??
              "No configuration selected"}
          </p>
          <button
            className={row}
            disabled={saving || !ready}
            onClick={() => void chooseCamilla()}
          >
            Choose CamillaDSP configuration…
          </button>
        </div>
      )}
      {(config.route === "hqplayer" || hqSetupOpen) && (
        <div className="mt-3 text-[11px] text-th-text-muted">
          <p>
            Local HQPlayer Desktop. Manage volume and processing in HQPlayer.
          </p>
          {unsupportedHqHost && (
            <p role="alert" className="text-red-400 mt-2 break-words">
              Stored endpoint {config.hqplayerHost}:{config.hqplayerPort} is not
              supported. Only local HQPlayer Desktop is supported. Choose “Use
              localhost” to change the endpoint explicitly.
            </p>
          )}
          <label htmlFor="hqplayer-port" className="block mt-2">
            Control port (localhost)
          </label>
          <div className="flex items-center gap-2 mt-1">
            <input
              id="hqplayer-port"
              type="number"
              min="1"
              max="65535"
              className={input}
              value={port}
              disabled={saving || !ready}
              onChange={(event) => setPort(event.target.value)}
            />
            <button
              className="text-th-accent disabled:opacity-50"
              disabled={
                saving ||
                !ready ||
                !/^\d+$/.test(port) ||
                Number(port) < 1 ||
                Number(port) > 65535 ||
                (config.route === "hqplayer" &&
                  !unsupportedHqHost &&
                  Number(port) === config.hqplayerPort)
              }
              onClick={() =>
                void save({
                  route: "hqplayer",
                  hqplayerPort: Number(port),
                  hqplayerHost: "127.0.0.1",
                })
              }
            >
              {unsupportedHqHost
                ? "Use localhost"
                : config.route === "hqplayer"
                  ? "Apply"
                  : "Use HQPlayer"}
            </button>
          </div>
        </div>
      )}
      {showDevices && (
        <div className="relative mt-2">
          <button
            type="button"
            className={`${row} justify-start`}
            disabled={devicesLoading}
            onClick={() => loadDevices(true)}
          >
            <RefreshCw
              size={12}
              className={devicesLoading ? "animate-spin" : ""}
            />
            {devicesLoading ? "Refreshing devices…" : "Refresh devices"}
          </button>
          {devicesError && (
            <p role="alert" className="text-[12px] text-red-400">
              Unable to load audio devices. Try refreshing.
            </p>
          )}
          {!devicesLoading && !devicesError && devices.length === 0 && (
            <p role="status" className="text-[12px] text-th-text-muted">
              No audio devices found.
            </p>
          )}
          <button
            className={`${input} flex items-center justify-between`}
            disabled={saving || !ready}
            onClick={() => setDeviceDropdownOpen((open) => !open)}
          >
            <span className="truncate">
              {devices.find((device) => device.id === config.device)?.name ??
                "Select device"}
            </span>
            <ChevronDown size={12} />
          </button>
          {deviceDropdownOpen && (
            <div className="mt-1 rounded-md border border-th-border-subtle bg-th-elevated max-h-[160px] overflow-y-auto">
              {devices.map((device) => (
                <button
                  key={device.id}
                  className={`${row} px-2.5`}
                  disabled={saving || !ready}
                  onClick={() => {
                    setDeviceDropdownOpen(false);
                    void save({ device: device.id });
                  }}
                >
                  {device.name}
                </button>
              ))}
            </div>
          )}
        </div>
      )}
    </section>
  );
}
