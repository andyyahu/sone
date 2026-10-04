import { useSyncExternalStore } from "react";

function subscribe(onChange: () => void) {
  document.addEventListener("visibilitychange", onChange);
  return () => document.removeEventListener("visibilitychange", onChange);
}

const getSnapshot = () => document.visibilityState !== "hidden";

/** Pause visual work when the WebView is hidden, without stopping audio. */
export function useDocumentVisible() {
  return useSyncExternalStore(subscribe, getSnapshot, () => true);
}
