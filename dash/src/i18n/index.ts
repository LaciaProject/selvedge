import i18n from "i18next";
import { initReactI18next } from "react-i18next";

import en from "./en.json";

// Two languages (RFC 0003 §8). English ships in the bundle; Chinese loads when
// chosen. The choice is a display preference, kept in localStorage.
export const LANGUAGES = ["en", "zh-CN"] as const;
export type Language = (typeof LANGUAGES)[number];
const STORAGE_KEY = "selvedge.language";

export function detectLanguage(): Language {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    if (saved && (LANGUAGES as readonly string[]).includes(saved)) return saved as Language;
  } catch {
    // Storage may be unavailable; fall back to the browser's languages.
  }
  const preferred = navigator.languages?.length ? navigator.languages : [navigator.language];
  return preferred.some((l) => l.toLowerCase().startsWith("zh")) ? "zh-CN" : "en";
}

async function load(lang: Language) {
  if (lang === "en" || i18n.hasResourceBundle(lang, "translation")) return;
  const messages = (await import("./zh-CN.json")).default;
  i18n.addResourceBundle(lang, "translation", messages);
}

export async function setLanguage(lang: Language) {
  await load(lang);
  await i18n.changeLanguage(lang);
  document.documentElement.lang = lang;
  try {
    localStorage.setItem(STORAGE_KEY, lang);
  } catch {
    // Not remembered; still switched.
  }
}

export async function initI18n() {
  const lang = detectLanguage();
  await i18n.use(initReactI18next).init({
    resources: { en: { translation: en } },
    lng: "en",
    fallbackLng: "en",
    interpolation: { escapeValue: false },
    returnNull: false,
  });
  await setLanguage(lang);
}

export default i18n;
