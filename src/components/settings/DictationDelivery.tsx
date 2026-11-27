import React from "react";
import { useTranslation } from "react-i18next";
import type { DictationTarget } from "@/bindings";
import { useSettings } from "../../hooks/useSettings";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
import { ToggleSwitch } from "../ui/ToggleSwitch";

export const DictationDelivery: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();

  return (
    <>
      <SettingContainer
        title={t("settings.general.dictationDelivery.targetLabel")}
        description={t("settings.general.dictationDelivery.targetDescription")}
        descriptionMode="inline"
        grouped
      >
        <Dropdown
          options={[
            {
              value: "recording_start",
              label: t("settings.general.dictationDelivery.recordingStart"),
            },
            {
              value: "recording_end",
              label: t("settings.general.dictationDelivery.recordingEnd"),
            },
          ]}
          selectedValue={getSetting("dictation_target") ?? "recording_start"}
          onSelect={(value) =>
            updateSetting("dictation_target", value as DictationTarget)
          }
          disabled={isUpdating("dictation_target")}
        />
      </SettingContainer>
      <ToggleSwitch
        checked={getSetting("notify_delivery_failure") ?? true}
        onChange={(enabled) =>
          updateSetting("notify_delivery_failure", enabled)
        }
        isUpdating={isUpdating("notify_delivery_failure")}
        label={t("settings.general.dictationDelivery.notifyLabel")}
        description={t("settings.general.dictationDelivery.notifyDescription")}
        descriptionMode="inline"
        grouped
      />
    </>
  );
};
