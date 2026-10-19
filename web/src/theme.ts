// The colour theme: the visitor's choice (Auto, Dark or Light) and the theme it resolves to, set as
// `data-theme` on <html> for styles.css. Auto follows `prefers-color-scheme`
// and keeps following it while the page is open. The choice is a per-browser
// convenience in localStorage; when storage is unavailable it is Auto.

import { createSignal } from "solid-js";

export type ThemeChoice = "auto" | "dark" | "light";
export const THEME_CHOICES: readonly ThemeChoice[] = ["auto", "dark", "light"];

const KEY = "judgebot-theme";
const light = window.matchMedia("(prefers-color-scheme: light)");

function stored(): ThemeChoice {
  try {
    const v = localStorage.getItem(KEY);
    return THEME_CHOICES.find((c) => c === v) ?? "auto";
  } catch {
    return "auto";
  }
}

const [choice, setChoiceSignal] = createSignal<ThemeChoice>(stored());

function apply(): void {
  const c = choice();
  const resolved = c === "auto" ? (light.matches ? "light" : "dark") : c;
  document.documentElement.dataset.theme = resolved;
}

light.addEventListener("change", apply);
apply();

/** The visitor's theme choice. */
export const themeChoice = choice;

/** Choose a theme, remembering it in this browser when storage allows. */
export function setThemeChoice(c: ThemeChoice): void {
  setChoiceSignal(c);
  try {
    if (c === "auto") localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, c);
  } catch {
    // Storage blocked: the choice lasts for this page only.
  }
  apply();
}
