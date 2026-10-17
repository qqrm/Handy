import React from "react";
import { createRoot } from "react-dom/client";
import { mockIPC } from "@tauri-apps/api/mocks";
import { Toaster } from "sonner";
import { PauseMediaWhileRecording } from "../../src/components/settings/PauseMediaWhileRecording";
import { useSettingsStore } from "../../src/stores/settingsStore";

// This fixture mounts the real control/store without the unrelated app startup.
// Only the Tauri IPC boundary is replaced; no native app or media runs here.
window.__TAURI_OS_PLUGIN_INTERNALS__ = {
  os_type: "macos",
  platform: "macos",
  family: "unix",
  arch: "aarch64",
  version: "15.0",
  eol: "\n",
  exe_extension: "",
};

const language = new URLSearchParams(window.location.search).get("lang") || "en";
let persisted = false;
let persistedTheme = "system";
const requested: boolean[] = [];
const unexpected: string[] = [];
const pending: Array<{
  enabled: boolean;
  resolve: (value: null) => void;
  reject: (reason: string) => void;
}> = [];

mockIPC((command, payload) => {
  if (command === "get_app_settings") {
    return {
      app_language: language,
      pause_media_while_recording: persisted,
      theme: persistedTheme,
    };
  }
  if (command === "change_theme_setting") {
    persistedTheme = (payload as { theme: string }).theme;
    return null;
  }
  if (command === "change_pause_media_while_recording_setting") {
    const enabled = (payload as { enabled: boolean }).enabled;
    requested.push(enabled);
    return new Promise<null>((resolve, reject) => {
      pending.push({ enabled, resolve, reject });
    });
  }
  unexpected.push(command);
  throw new Error(`Unexpected fixture IPC: ${command}`);
});

const { default: i18n } = await import("../../src/i18n");
await i18n.changeLanguage(language);
await useSettingsStore.getState().refreshSettings();

const controls = {
  snapshot: () => ({
    persisted,
    requested: [...requested],
    pending: pending.length,
    unexpected: [...unexpected],
    setting: useSettingsStore.getState().getSetting("pause_media_while_recording"),
    theme: useSettingsStore.getState().getSetting("theme"),
    persistedTheme,
  }),
  settle: (outcome: "allow" | "deny") => {
    const request = pending.shift();
    if (!request) throw new Error("No pending settings command");
    if (outcome === "deny") {
      // Rust's string Err reaches the generated binding as Result.status=error.
      request.reject("QuickTime Automation permission denied");
    } else {
      persisted = request.enabled;
      request.resolve(null);
    }
  },
  refresh: () => useSettingsStore.getState().refreshSettings(),
  changeTheme: () => useSettingsStore.getState().updateSetting("theme", "dark"),
};

declare global {
  interface Window {
    mediaPauseConsentFixture: typeof controls;
  }
}

window.mediaPauseConsentFixture = controls;
createRoot(document.getElementById("root")!).render(
  <>
    <PauseMediaWhileRecording descriptionMode="inline" />
    <Toaster duration={60_000} />
  </>,
);
