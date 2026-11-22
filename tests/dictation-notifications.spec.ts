import { expect, test, type Page } from "@playwright/test";
import en from "../src/i18n/locales/en/translation.json" with { type: "json" };
import ru from "../src/i18n/locales/ru/translation.json" with { type: "json" };
import type {} from "./fixtures/dictation-notifications";

async function expectUnclipped(page: Page) {
  const bounds = await page
    .locator(".notice, .notice h1, .notice p")
    .evaluateAll((elements) =>
      elements.map((element) => {
        const rect = element.getBoundingClientRect();
        return {
          element: element.tagName,
          withinWindow:
            rect.left >= 0 &&
            rect.top >= 0 &&
            rect.right <= innerWidth &&
            rect.bottom <= innerHeight,
          withinOwnBox:
            element.scrollHeight <= element.clientHeight + 1 &&
            element.scrollWidth <= element.clientWidth + 1,
        };
      }),
    );
  expect(bounds).toHaveLength(3);
  for (const element of bounds) {
    expect(
      element,
      "notification text must fit the real popup dimensions",
    ).toMatchObject({ withinWindow: true, withinOwnBox: true });
  }
}

for (const [language, strings] of Object.entries({ en, ru })) {
  test(`actual notification entry handles readiness and delivery outcomes (${language})`, async ({
    page,
  }, testInfo) => {
    await page.setViewportSize({ width: 420, height: 170 });
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(
      `/tests/fixtures/dictation-notifications.html?lang=${language}`,
    );

    const text = strings.notifications;
    await expect(page.getByRole("heading")).toHaveText(text.recordingTitle);
    await expect(page.locator(".notice p")).toHaveText(
      text.recordingBody.replace("{{duration}}", "3:00"),
    );
    expect(
      (await page.evaluate(() => window.notificationFixture.snapshot())).ready,
    ).toBeGreaterThan(0);
    await expectUnclipped(page);

    const outcomes = [
      {
        notice: { kind: "delivery_fallback", reason: "uncertain" },
        title: text.deliveryFallbackTitle,
        body: text.deliveryUncertainBody,
      },
      {
        notice: { kind: "clipboard_error" },
        title: text.clipboardErrorTitle,
        body: text.clipboardErrorBody,
      },
      {
        notice: { kind: "auto_submit_skipped" },
        title: text.autoSubmitSkippedTitle,
        body: text.autoSubmitSkippedBody,
      },
      {
        notice: { kind: "delivery_fallback", reason: "unavailable" },
        title: text.deliveryFallbackTitle,
        body: `${text.targetUnavailable} ${text.deliveryFallbackBody}`,
      },
    ];
    for (const outcome of outcomes) {
      await page.evaluate(
        (notice) => window.notificationFixture.show(notice),
        outcome.notice,
      );
      await expect(page.getByRole("heading")).toHaveText(outcome.title);
      await expect(page.locator(".notice p")).toHaveText(outcome.body);
      await expectUnclipped(page);
    }
    expect(
      (await page.evaluate(() => window.notificationFixture.snapshot()))
        .unexpected,
    ).toEqual([]);
    expect(errors).toEqual([]);
    if (language === "ru") {
      await page.screenshot({
        path: testInfo.outputPath("notification-ru.png"),
        omitBackground: true,
      });
    }
  });
}
