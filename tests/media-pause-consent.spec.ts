import { expect, test } from "@playwright/test";
import en from "../src/i18n/locales/en/translation.json" with { type: "json" };
import ru from "../src/i18n/locales/ru/translation.json" with { type: "json" };
import type {} from "./fixtures/media-pause-consent";

for (const [language, strings] of Object.entries({ en, ru })) {
  test(`macOS media consent rolls back denial and persists success (${language})`, async ({
    page,
  }) => {
    const text = strings.settings.debug.pauseMediaWhileRecording;
    await page.goto(
      `/tests/fixtures/media-pause-consent.html?lang=${language}`,
    );
    const snapshot = () =>
      page.evaluate(() => window.mediaPauseConsentFixture.snapshot());
    const toggle = page.getByRole("checkbox");
    await expect(
      page.getByText(text.macosDescription, { exact: true }),
    ).toBeVisible();
    await expect(toggle).not.toBeChecked();

    await toggle.check();
    await expect(toggle).toBeChecked();
    await expect(toggle).toBeDisabled();
    expect(await snapshot()).toMatchObject({
      persisted: false,
      requested: [true],
      pending: 1,
    });
    // A separate setting finishes while the consent command is still pending.
    // Rejecting consent must roll back only the media-pause setting.
    await page.evaluate(() => window.mediaPauseConsentFixture.changeTheme());
    expect(await snapshot()).toMatchObject({
      pending: 1,
      theme: "dark",
      persistedTheme: "dark",
    });
    await page.evaluate(() => window.mediaPauseConsentFixture.settle("deny"));
    await expect(
      page.getByText(text.macosPermissionError, { exact: true }),
    ).toBeVisible();
    await expect(toggle).not.toBeChecked();
    await expect(toggle).toBeEnabled();
    expect(await snapshot()).toMatchObject({
      persisted: false,
      setting: false,
      theme: "dark",
      persistedTheme: "dark",
    });

    await toggle.check();
    await expect(toggle).toBeDisabled();
    await page.evaluate(() => window.mediaPauseConsentFixture.settle("allow"));
    await expect(toggle).toBeChecked();
    await expect(toggle).toBeEnabled();
    await page.evaluate(() => window.mediaPauseConsentFixture.refresh());
    await expect(toggle).toBeChecked();
    expect(await snapshot()).toMatchObject({
      persisted: true,
      setting: true,
    });

    await toggle.uncheck();
    await expect(toggle).toBeDisabled();
    // Disabling must pass false, never another enabling/consent request.
    // Native permission prompting is outside this mocked frontend test.
    expect(await snapshot()).toMatchObject({
      requested: [true, true, false],
      pending: 1,
    });
    await page.evaluate(() => window.mediaPauseConsentFixture.settle("allow"));
    await expect(toggle).not.toBeChecked();
    await expect(toggle).toBeEnabled();
    await page.evaluate(() => window.mediaPauseConsentFixture.refresh());
    await expect(toggle).not.toBeChecked();
    expect(await snapshot()).toEqual({
      persisted: false,
      requested: [true, true, false],
      pending: 0,
      unexpected: [],
      setting: false,
      theme: "dark",
      persistedTheme: "dark",
    });
  });
}
