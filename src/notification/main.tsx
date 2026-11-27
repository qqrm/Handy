import React, { useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
import { emit, listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useTranslation } from "react-i18next";
import { ClipboardCheck, Mic, TriangleAlert } from "lucide-react";
import i18n from "@/i18n";
import {
  applyTheme,
  getStoredTheme,
  syncThemeFromSettings,
} from "@/lib/utils/theme";
import type { Theme } from "@/bindings";
import "./notification.css";

type Notice = (
  | { kind: "recording"; elapsed_seconds: number }
  | {
      kind: "delivery_fallback";
      reason: "unavailable" | "changed" | "unsupported" | "uncertain";
    }
  | { kind: "clipboard_error" }
  | { kind: "auto_submit_skipped" }
) & { language: string; theme: Theme };

const reasons = {
  unavailable: "notifications.targetUnavailable",
  changed: "notifications.targetChanged",
  unsupported: "notifications.targetUnsupported",
} as const;

function Notification() {
  const { t } = useTranslation();
  const [notice, setNotice] = useState<Notice | null>(null);

  useEffect(() => {
    let disposed = false;
    let unlisten: UnlistenFn | undefined;
    void listen<Notice>("handy-notification", (event) => {
      applyTheme(event.payload.theme);
      void i18n.changeLanguage(event.payload.language);
      setNotice(event.payload);
    }).then((stop) => {
      if (disposed) {
        stop();
        return;
      }
      unlisten = stop;
      void emit("handy-notification-ready");
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  if (!notice) return null;
  const recording = notice.kind === "recording";
  const clipboardError = notice.kind === "clipboard_error";
  const submitSkipped = notice.kind === "auto_submit_skipped";
  const Icon = recording
    ? Mic
    : clipboardError
      ? TriangleAlert
      : ClipboardCheck;
  const title = recording
    ? t("notifications.recordingTitle")
    : clipboardError
      ? t("notifications.clipboardErrorTitle")
      : submitSkipped
        ? t("notifications.autoSubmitSkippedTitle")
        : t("notifications.deliveryFallbackTitle");
  let body: string;
  if (notice.kind === "recording") {
    const minutes = Math.floor(notice.elapsed_seconds / 60);
    const seconds = (notice.elapsed_seconds % 60).toString().padStart(2, "0");
    body = t("notifications.recordingBody", {
      duration: `${minutes}:${seconds}`,
    });
  } else if (notice.kind === "clipboard_error") {
    body = t("notifications.clipboardErrorBody");
  } else if (notice.kind === "auto_submit_skipped") {
    body = t("notifications.autoSubmitSkippedBody");
  } else if (notice.reason === "uncertain") {
    body = t("notifications.deliveryUncertainBody");
  } else {
    body = `${t(reasons[notice.reason])} ${t("notifications.deliveryFallbackBody")}`;
  }

  return (
    <section
      className={`notice ${recording ? "recording" : ""}`}
      role="status"
      aria-live="polite"
    >
      <div className="notice-icon">
        <Icon size={25} aria-hidden="true" />
      </div>
      <div className="notice-copy">
        <h1>{title}</h1>
        <p>{body}</p>
      </div>
    </section>
  );
}

applyTheme(getStoredTheme());
void syncThemeFromSettings();
void listen<Theme>("theme-changed", (event) => applyTheme(event.payload));

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <Notification />
  </React.StrictMode>,
);
