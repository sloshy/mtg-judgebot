// The page's two remembered settings: the colour theme (in the bar) and the
// feed's layout (over the feed).

import { createSignal, For, Match, Switch } from "solid-js";
import type { Messages } from "./messages";
import { setThemeChoice, THEME_CHOICES, themeChoice } from "./theme";

/** Auto, Dark or Light: an icon for the current choice over a native select,
 * which shows its text on wide screens and is the whole control on phones. */
export function ThemeSelect(props: { m: Messages }) {
  return (
    <label class="theme-select">
      <ThemeIcon />
      <span class="sr-only">{props.m.themeLabel}</span>
      <select
        value={themeChoice()}
        onChange={(e) => {
          const v = THEME_CHOICES.find((c) => c === e.currentTarget.value);
          if (v) setThemeChoice(v);
        }}
      >
        <For each={THEME_CHOICES}>{(c) => <option value={c}>{props.m.themeName[c]}</option>}</For>
      </select>
      <svg class="theme-caret" viewBox="0 0 16 16" width="12" height="12" aria-hidden="true">
        <path
          d="m4 6 4 4 4-4"
          fill="none"
          stroke="currentColor"
          stroke-width="1.8"
          stroke-linecap="round"
          stroke-linejoin="round"
        />
      </svg>
    </label>
  );
}

function ThemeIcon() {
  return (
    <svg class="theme-icon" viewBox="0 0 20 20" width="18" height="18" aria-hidden="true">
      <Switch>
        <Match when={themeChoice() === "light"}>
          <circle cx="10" cy="10" r="3.6" fill="none" stroke="currentColor" stroke-width="1.8" />
          <path
            d="M10 1.8v2M10 16.2v2M1.8 10h2M16.2 10h2M4.2 4.2l1.4 1.4M14.4 14.4l1.4 1.4M4.2 15.8l1.4-1.4M14.4 5.6l1.4-1.4"
            stroke="currentColor"
            stroke-width="1.8"
            stroke-linecap="round"
          />
        </Match>
        <Match when={themeChoice() === "dark"}>
          <path
            d="M16.5 12.2A7 7 0 0 1 7.8 3.5a7 7 0 1 0 8.7 8.7z"
            fill="none"
            stroke="currentColor"
            stroke-width="1.8"
            stroke-linejoin="round"
          />
        </Match>
        <Match when={themeChoice() === "auto"}>
          <circle cx="10" cy="10" r="7" fill="none" stroke="currentColor" stroke-width="1.8" />
          <path d="M10 3a7 7 0 0 1 0 14z" fill="currentColor" />
        </Match>
      </Switch>
    </svg>
  );
}

/** The feed's two layouts: a line per question that opens in place, or every
 * exchange in full. */
export type View = "compact" | "expanded";
const VIEW_KEY = "judgebot-feed-view";

function storedView(): View {
  try {
    return localStorage.getItem(VIEW_KEY) === "expanded" ? "expanded" : "compact";
  } catch {
    return "compact";
  }
}

const [view, setViewSignal] = createSignal<View>(storedView());

/** The feed's layout, remembered in this browser when storage allows. */
export const feedView = view;

function setView(v: View): void {
  setViewSignal(v);
  try {
    localStorage.setItem(VIEW_KEY, v);
  } catch {
    // Storage blocked: the layout lasts for this page only.
  }
}

/** Compact | Expanded, as two pressed-or-not buttons. */
export function ViewSwitch(props: { m: Messages }) {
  return (
    <fieldset class="view-switch">
      <legend class="sr-only">{props.m.viewLabel}</legend>
      <button type="button" aria-pressed={view() === "compact"} onClick={() => setView("compact")}>
        <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
          <path
            d="M2.5 4h11M2.5 8h11M2.5 12h11"
            stroke="currentColor"
            stroke-width="1.6"
            stroke-linecap="round"
          />
        </svg>
        {props.m.viewCompact}
      </button>
      <button
        type="button"
        aria-pressed={view() === "expanded"}
        onClick={() => setView("expanded")}
      >
        <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
          <rect
            x="2.5"
            y="2.5"
            width="11"
            height="4.5"
            rx="1.2"
            fill="none"
            stroke="currentColor"
            stroke-width="1.5"
          />
          <rect
            x="2.5"
            y="9"
            width="11"
            height="4.5"
            rx="1.2"
            fill="none"
            stroke="currentColor"
            stroke-width="1.5"
          />
        </svg>
        {props.m.viewExpanded}
      </button>
    </fieldset>
  );
}
