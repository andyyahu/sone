import { LogOut, Keyboard, X, Settings, Info } from "lucide-react";
import { useState, useEffect, useRef } from "react";
import { useAtom, useAtomValue } from "jotai";
import { useAuth } from "../hooks/useAuth";
import { useNavigation } from "../hooks/useNavigation";
import { useEscapeDismiss } from "../hooks/useEscapeDismiss";
import { DISMISS_PRIORITY } from "../lib/dismissStack";
import { currentUserAvatarAtom } from "../atoms/auth";
import {
  ACTION_REGISTRY,
  FIXED_KEY_DOCS,
  DEFAULT_BINDINGS,
  shortcutsAtom,
  formatCombo,
  keyFromEvent,
  comboEquals,
  isReserved,
  type ActionId,
  type KeyCombo,
} from "../lib/shortcuts";
import SettingsSheet, { type TabId } from "./settings/SettingsSheet";
import { OPEN_SETTINGS_EVENT } from "./ProxyNoticeBanner";
import AboutModal from "./AboutModal";
import AudioOutputSettings from "./settings/AudioOutputSettings";
import TidalImage from "./TidalImage";

export default function UserMenu() {
  const { userName, logout } = useAuth();
  const { navigateToProfile } = useNavigation();
  const avatarUrl = useAtomValue(currentUserAvatarAtom);
  const [open, setOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  // Set only by a deep link, and cleared when that sheet closes: undefined
  // means "whatever the sheet's own default is", so opening Settings from this
  // menu behaves exactly as it did before deep-linking existed.
  const [settingsTab, setSettingsTab] = useState<TabId | undefined>(undefined);
  const [shortcutsOpen, setShortcutsOpen] = useState(false);
  const [aboutOpen, setAboutOpen] = useState(false);
  const [bindings, setBindings] = useAtom(shortcutsAtom);
  const [editingId, setEditingId] = useState<ActionId | null>(null);
  const [reservedHint, setReservedHint] = useState(false);
  // The proxy banner renders above this menu and cannot reach the sheet's
  // state, so it asks. Only meaningful inside the authenticated shell — the
  // pre-login banner has no settings screen to send anyone to, which is the
  // whole reason it carries a disable button.
  useEffect(() => {
    const onOpen = (e: Event) => {
      const tab = (e as CustomEvent<TabId | undefined>).detail;
      if (tab) setSettingsTab(tab);
      setSettingsOpen(true);
    };
    window.addEventListener(OPEN_SETTINGS_EVENT, onOpen);
    return () => window.removeEventListener(OPEN_SETTINGS_EVENT, onOpen);
  }, []);
  const menuRef = useRef<HTMLDivElement>(null);

  // Toggle shortcuts modal from ? key
  useEffect(() => {
    // Close the dropdown too — left open it sits above the modal on the
    // dismissal stack and eats the first Escape.
    const handler = () => {
      setOpen(false);
      setShortcutsOpen((prev) => !prev);
    };
    window.addEventListener("toggle-shortcuts", handler);
    return () => window.removeEventListener("toggle-shortcuts", handler);
  }, []);

  // Capture next keydown while editing a shortcut row
  useEffect(() => {
    if (!editingId) return;

    const handler = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();

      if (e.code === "Escape") {
        setEditingId(null);
        setReservedHint(false);
        return;
      }

      const combo = keyFromEvent(e);
      if (!combo) return; // pure modifier — keep capturing

      if (isReserved(combo)) {
        setReservedHint(true);
        return;
      }

      const next: Record<ActionId, KeyCombo | null> = { ...bindings };
      for (const id of Object.keys(next) as ActionId[]) {
        if (id !== editingId && comboEquals(next[id], combo)) {
          next[id] = null;
        }
      }
      next[editingId] = combo;
      setBindings(next);
      setEditingId(null);
      setReservedHint(false);
    };

    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, [editingId, bindings, setBindings]);

  // Close on click outside
  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  useEscapeDismiss(
    open,
    () => {
      setOpen(false);
    },
    DISMISS_PRIORITY.contextMenu,
  );

  useEscapeDismiss(
    shortcutsOpen,
    () => {
      setShortcutsOpen(false);
      setEditingId(null);
      setReservedHint(false);
    },
    DISMISS_PRIORITY.modal,
  );

  const menuItemClass =
    "w-full flex items-center gap-3 px-4 py-2.5 text-[13px] text-th-text-secondary hover:text-th-text-primary hover:bg-th-border-subtle transition-colors";

  return (
    <div ref={menuRef} className="relative">
      <button
        onClick={() => setOpen((prev) => !prev)}
        className="w-8 h-8 rounded-full bg-th-button hover:bg-th-button-hover flex items-center justify-center transition-colors overflow-hidden"
        title="Account"
      >
        <TidalImage
          src={avatarUrl ?? undefined}
          alt=""
          type="artist"
          className="w-full h-full"
        />
      </button>

      {open && (
        <div className="absolute right-0 top-full mt-2 w-80 max-h-[80vh] overflow-y-auto bg-th-surface rounded-lg shadow-2xl shadow-black/60 border border-th-border-subtle z-50 py-1 animate-fadeIn">
          {/* User info — navigates to profile */}
          <button
            onClick={() => {
              navigateToProfile();
              setOpen(false);
            }}
            className="w-full px-4 py-3 border-b border-th-border-subtle hover:bg-th-border-subtle transition-colors"
          >
            <div className="flex items-center gap-3">
              <div className="w-9 h-9 rounded-full bg-th-button overflow-hidden shrink-0">
                <TidalImage
                  src={avatarUrl ?? undefined}
                  alt=""
                  type="artist"
                  className="w-full h-full"
                />
              </div>
              <div className="min-w-0 flex-1 text-left">
                <p className="text-[13px] font-medium text-th-text-primary truncate">
                  {userName}
                </p>
              </div>
            </div>
          </button>

          <AudioOutputSettings />

          {/* ── Settings ── */}
          <div className="border-t border-th-border-subtle my-1" />

          <button
            onClick={() => {
              setOpen(false);
              setSettingsOpen(true);
            }}
            className={menuItemClass}
          >
            <Settings size={16} />
            Settings
          </button>

          {/* ── Shortcuts ── */}
          <div className="border-t border-th-border-subtle my-1" />

          <button
            onClick={() => {
              setOpen(false);
              setShortcutsOpen(true);
            }}
            className={menuItemClass}
          >
            <Keyboard size={16} />
            Shortcuts
          </button>

          {/* ── About ── */}
          <div className="border-t border-th-border-subtle my-1" />

          <button
            onClick={() => {
              setOpen(false);
              setAboutOpen(true);
            }}
            className={menuItemClass}
          >
            <Info size={16} />
            About
          </button>

          {/* ── Logout ── */}
          <div className="border-t border-th-border-subtle my-1" />
          <button
            onClick={() => {
              setOpen(false);
              logout();
            }}
            className="w-full flex items-center gap-3 px-4 py-2.5 text-[13px] text-red-400 hover:bg-th-border-subtle transition-colors"
          >
            <LogOut size={16} />
            Log out
          </button>
        </div>
      )}

      <SettingsSheet
        open={settingsOpen}
        onClose={() => {
          setSettingsOpen(false);
          setSettingsTab(undefined);
        }}
        initialTab={settingsTab}
      />
      <AboutModal open={aboutOpen} onClose={() => setAboutOpen(false)} />

      {/* Shortcuts modal */}
      {shortcutsOpen && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
          onClick={() => {
            setShortcutsOpen(false);
            setEditingId(null);
            setReservedHint(false);
          }}
        >
          <div
            className="bg-th-elevated rounded-xl shadow-2xl w-[460px] max-h-[80vh] flex flex-col overflow-hidden"
            onClick={(e) => e.stopPropagation()}
            style={{ animation: "slideUp 0.2s var(--ease-settle)" }}
          >
            <div className="flex items-center justify-between px-5 pt-5 pb-3">
              <h2 className="text-[16px] font-bold text-th-text-primary">
                Keyboard Shortcuts
              </h2>
              <button
                onClick={() => {
                  setShortcutsOpen(false);
                  setEditingId(null);
                  setReservedHint(false);
                }}
                className="w-8 h-8 rounded-full flex items-center justify-center hover:bg-th-inset transition-colors text-th-text-muted hover:text-th-text-primary"
              >
                <X size={18} />
              </button>
            </div>
            <div className="px-5 pb-3 flex flex-col gap-0.5 overflow-y-auto min-h-0">
              {ACTION_REGISTRY.map((action) => {
                const isEditing = editingId === action.id;
                const binding = action.fixed
                  ? action.default
                  : bindings[action.id];
                return (
                  <div
                    key={action.id}
                    title={action.fixed ? "Not rebindable" : undefined}
                    onDoubleClick={
                      action.fixed
                        ? undefined
                        : () => {
                            setReservedHint(false);
                            setEditingId(action.id);
                          }
                    }
                    className={`flex items-center justify-between py-2 px-2 rounded select-none ${
                      action.fixed
                        ? "opacity-60"
                        : "hover:bg-th-inset cursor-pointer"
                    }`}
                  >
                    <span className="text-[13px] text-th-text-secondary">
                      {action.label}
                    </span>
                    <kbd
                      className={`text-[12px] font-mono px-2.5 py-1 rounded-md border transition-colors ${
                        isEditing
                          ? reservedHint
                            ? "bg-red-500/10 text-red-400 border-red-500/40"
                            : "bg-th-accent/10 text-th-accent border-th-accent/60 animate-pulse"
                          : "bg-th-surface text-th-text-muted border-th-border-subtle"
                      }`}
                    >
                      {isEditing
                        ? reservedHint
                          ? "Reserved — pick another"
                          : "Press a key…"
                        : formatCombo(binding)}
                    </kbd>
                  </div>
                );
              })}
              {FIXED_KEY_DOCS.map((doc) => (
                <div
                  key={doc.label}
                  title="Not rebindable"
                  className="flex items-center justify-between py-2 px-2 rounded select-none opacity-60"
                >
                  <span className="text-[13px] text-th-text-secondary">
                    {doc.label}
                  </span>
                  <kbd className="text-[12px] font-mono px-2.5 py-1 rounded-md border bg-th-surface text-th-text-muted border-th-border-subtle">
                    {formatCombo(doc.combo)}
                  </kbd>
                </div>
              ))}
            </div>
            <div className="border-t border-th-border-subtle px-5 py-3 flex justify-between items-center">
              <span className="text-[11px] text-th-text-muted">
                Double-click to edit · Esc to cancel
              </span>
              <button
                onClick={() => {
                  setBindings(DEFAULT_BINDINGS);
                  setEditingId(null);
                  setReservedHint(false);
                }}
                className="text-[12px] px-3 py-1.5 rounded-md bg-th-surface hover:bg-th-border-subtle text-th-text-secondary transition-colors"
              >
                Restore defaults
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
