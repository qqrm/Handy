import { emit, listen } from "@tauri-apps/api/event";
import { mockIPC } from "@tauri-apps/api/mocks";

// Only the Tauri IPC/event transport is fake. The real notification entry point
// below mounts the production component and CSS, not a copied test component.
const language =
  new URLSearchParams(window.location.search).get("lang") || "en";
const unexpected: string[] = [];
let ready = 0;

mockIPC(
  (command) => {
    if (command === "get_app_settings") {
      return { app_language: language, theme: "dark" };
    }
    unexpected.push(command);
    throw new Error(`Unexpected notification fixture IPC: ${command}`);
  },
  { shouldMockEvents: true },
);

const show = (notice: Record<string, unknown>) =>
  emit("handy-notification", { ...notice, language, theme: "dark" });

await listen("handy-notification-ready", () => {
  ready += 1;
  // An immediate response proves the real entry subscribes before announcing
  // readiness; announcing too soon would lose this first notification.
  void show({ kind: "recording", elapsed_seconds: 180 });
});

const controls = {
  show,
  snapshot: () => ({ ready, unexpected: [...unexpected] }),
};

declare global {
  interface Window {
    notificationFixture: typeof controls;
  }
}

window.notificationFixture = controls;
await import("../../src/notification/main");
