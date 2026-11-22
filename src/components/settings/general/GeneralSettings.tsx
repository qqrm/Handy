import React from "react";
import { useTranslation } from "react-i18next";
import { type } from "@tauri-apps/plugin-os";
import { MicrophoneSelector } from "../MicrophoneSelector";
import { ChannelSelector } from "../ChannelSelector";
import { ShortcutInput } from "../ShortcutInput";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { OutputDeviceSelector } from "../OutputDeviceSelector";
import { ShortcutActivationSetting } from "../ShortcutActivation";
import { AudioFeedback } from "../AudioFeedback";
import { useSettings } from "../../../hooks/useSettings";
import { VolumeSlider } from "../VolumeSlider";
import { MuteWhileRecording } from "../MuteWhileRecording";
import { PauseMediaWhileRecording } from "../PauseMediaWhileRecording";
import { ModelSettingsCard } from "./ModelSettingsCard";
import { DictationDelivery } from "../DictationDelivery";
import { RecordingReminders } from "../RecordingReminders";

export const GeneralSettings: React.FC = () => {
  const { t } = useTranslation();
  const { audioFeedbackEnabled, getSetting } = useSettings();
  const platform = type();
  const isLinux = platform === "linux";
  const soundEnabled =
    audioFeedbackEnabled ||
    (getSetting("recording_reminders_enabled") &&
      getSetting("recording_reminder_sound"));
  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <SettingsGroup title={t("settings.general.title")}>
        <ShortcutInput shortcutId="transcribe" grouped={true} />
        {!isLinux && (
          <ShortcutInput shortcutId="transcribe_to_clipboard" grouped={true} />
        )}
        <ShortcutActivationSetting descriptionMode="tooltip" grouped={true} />
        {/* Cancel shortcut remains hidden on Linux because of dynamic shortcut instability. */}
        {!isLinux && <ShortcutInput shortcutId="cancel" grouped={true} />}
      </SettingsGroup>
      {platform === "windows" && (
        <SettingsGroup title={t("settings.general.dictationDelivery.title")}>
          <DictationDelivery />
        </SettingsGroup>
      )}
      <SettingsGroup title={t("settings.general.recordingReminders.title")}>
        <RecordingReminders />
      </SettingsGroup>
      <ModelSettingsCard />
      <SettingsGroup title={t("settings.sound.title")}>
        <MicrophoneSelector descriptionMode="tooltip" grouped={true} />
        <ChannelSelector descriptionMode="tooltip" grouped={true} />
        <MuteWhileRecording descriptionMode="tooltip" grouped={true} />
        {(platform === "windows" || isLinux || platform === "macos") && (
          <PauseMediaWhileRecording descriptionMode="tooltip" grouped={true} />
        )}
        <AudioFeedback descriptionMode="tooltip" grouped={true} />
        <OutputDeviceSelector
          descriptionMode="tooltip"
          grouped={true}
          disabled={!soundEnabled}
        />
        <VolumeSlider disabled={!soundEnabled} />
      </SettingsGroup>
    </div>
  );
};
