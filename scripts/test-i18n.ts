import assert from "node:assert/strict";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createInstance } from "i18next";
import {
  I18nextProvider,
  initReactI18next,
  useTranslation,
} from "react-i18next";
import en from "../src/i18n/locales/en/translation.json";
import ru from "../src/i18n/locales/ru/translation.json";

const i18n = createInstance();
await i18n.use(initReactI18next).init({
  resources: { en: { translation: en }, ru: { translation: ru } },
  lng: "en",
  fallbackLng: "en",
  interpolation: { escapeValue: false },
  react: { useSuspense: false },
});

const ModelCount = () => {
  const { t } = useTranslation();
  return createElement(
    "span",
    null,
    t("onboarding.showAllModels", { total: 7 }),
  );
};
const render = () =>
  renderToStaticMarkup(
    createElement(I18nextProvider, { i18n }, createElement(ModelCount)),
  );

assert.equal(i18n.t("sidebar.general"), "General");
assert.equal(render(), "<span>Show all 7 models</span>");
await i18n.changeLanguage("ru");
assert.equal(i18n.t("sidebar.general"), "Общие");
assert.equal(render(), "<span>Показать все модели (7)</span>");
await i18n.changeLanguage("unsupported-test-locale");
assert.equal(render(), "<span>Show all 7 models</span>");
console.log("i18n: bundled EN/RU, React interpolation and fallback passed");
