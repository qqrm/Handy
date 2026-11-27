import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { useSettings } from "../../hooks/useSettings";
import { SettingContainer } from "../ui/SettingContainer";
import { ToggleSwitch } from "../ui/ToggleSwitch";

export const RecordingReminders: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();
  const enabled = getSetting("recording_reminders_enabled") ?? false;
  const minutes = getSetting("recording_reminder_minutes") ?? 1;
  const [minutesInput, setMinutesInput] = useState(String(minutes));

  useEffect(() => {
    setMinutesInput(String(minutes));
  }, [minutes]);

  const saveMinutes = () => {
    const value = Number(minutesInput);
    if (Number.isInteger(value) && value >= 1 && value <= 120) {
      void updateSetting("recording_reminder_minutes", value);
    } else {
      setMinutesInput(String(minutes));
    }
  };

  return (
    <>
      <ToggleSwitch
        checked={enabled}
        onChange={(value) =>
          updateSetting("recording_reminders_enabled", value)
        }
        isUpdating={isUpdating("recording_reminders_enabled")}
        label={t("settings.general.recordingReminders.enabledLabel")}
        description={t(
          "settings.general.recordingReminders.enabledDescription",
        )}
        descriptionMode="inline"
        grouped
      />
      <SettingContainer
        title={t("settings.general.recordingReminders.minutesLabel")}
        description={t(
          "settings.general.recordingReminders.minutesDescription",
          {
            first: minutes,
            second: minutes * 3,
            third: minutes * 7,
          },
        )}
        descriptionMode="inline"
        disabled={!enabled}
        grouped
      >
        <input
          type="number"
          min={1}
          max={120}
          step={1}
          aria-label={t("settings.general.recordingReminders.minutesLabel")}
          className="w-20 rounded-md border border-mid-gray/30 bg-background px-2 py-1 text-right text-sm focus:outline-none focus:ring-2 focus:ring-logo-primary disabled:opacity-50"
          value={minutesInput}
          onChange={(event) => setMinutesInput(event.target.value)}
          onBlur={saveMinutes}
          onKeyDown={(event) => {
            if (event.key === "Enter") event.currentTarget.blur();
          }}
          disabled={!enabled || isUpdating("recording_reminder_minutes")}
        />
      </SettingContainer>
      <ToggleSwitch
        checked={getSetting("recording_reminder_sound") ?? false}
        onChange={(value) => updateSetting("recording_reminder_sound", value)}
        isUpdating={isUpdating("recording_reminder_sound")}
        disabled={!enabled}
        label={t("settings.general.recordingReminders.soundLabel")}
        description={t("settings.general.recordingReminders.soundDescription")}
        descriptionMode="inline"
        grouped
      />
    </>
  );
};
