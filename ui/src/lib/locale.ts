// Language and formatter of this page, fixed at load time from the browser.

import { messages, pickLang, pickLocale } from "../i18n";
import { createFormatter } from "./format";

const languages = typeof navigator === "undefined" ? [] : navigator.languages;

export const lang = pickLang(languages);
export const t = messages[lang];
export const f = createFormatter(lang, pickLocale(lang, languages));
