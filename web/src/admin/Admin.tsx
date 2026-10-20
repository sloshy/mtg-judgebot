// The admin pages: the operator's view of every stored call, the feedback
// queue, the failed calls, and the usage and spend. Served by the --admin
// listener only (ADMIN_ADDR, loopback), behind ADMIN_TOKEN
// (docs/DECISIONS.md D37, D38).
//
// The shell is the public page's bar with the admin tabs; each view is its
// own file.

import "@fontsource-variable/figtree";
import { createEffect, createSignal, For, Match, onCleanup, Show, Switch } from "solid-js";
import { BotAvatar, BotMessage, Notice } from "../Chat";
import { ThemeSelect } from "../Controls";
import { type Messages, messages, pickLocale } from "../messages";
import { type AdminView, follow, href, route } from "../router";
import "./admin.css";
import { forgetToken, type Read, takeToken } from "./api";
import { CallPage } from "./Call";
import { CallsPage } from "./Calls";
import { FailuresPage } from "./Failures";
import { FeedbackPage, ItemPage } from "./Feedback";
import { type AdminMessages, adminMessages } from "./messages";
import { UsagePage } from "./Usage";

/** What every view needs: the token, both catalogs, and how to report a
 * refused token. */
export interface AdminContext {
  token: string;
  m: AdminMessages;
  /** The public page's catalog, for the pieces shared with it (an answer). */
  pub: Messages;
  refused: () => void;
}

/** The tabs, in order. */
const TABS: readonly {
  view: AdminView;
  label: (m: AdminMessages) => string;
  on: AdminView["view"][];
}[] = [
  { view: { view: "calls", filter: {} }, label: (m) => m.navCalls, on: ["calls", "call"] },
  {
    view: { view: "feedback", filter: { status: "open" } },
    label: (m) => m.navFeedback,
    on: ["feedback", "item"],
  },
  { view: { view: "failures" }, label: (m) => m.navFailures, on: ["failures"] },
  { view: { view: "usage" }, label: (m) => m.navUsage, on: ["usage"] },
];

export default function Admin() {
  const locale = pickLocale(navigator.languages, "en" as const);
  const m = adminMessages(locale);
  const pub = messages(locale);
  const [token, setToken] = createSignal(takeToken());
  const [refused, setRefused] = createSignal(false);
  document.documentElement.lang = locale;
  document.title = `${m.title} · ${pub.title}`;
  // The pages are private: never offered to a search engine, whatever the
  // listener's address.
  const robots = document.createElement("meta");
  robots.name = "robots";
  robots.content = "noindex";
  document.head.append(robots);

  document.addEventListener("click", follow);
  onCleanup(() => document.removeEventListener("click", follow));

  const view = (): AdminView => {
    const r = route();
    return r.page === "admin" ? r.view : { view: "missing" };
  };
  createEffect(() => {
    view();
    // A link followed while a token was refused tries the stored one again.
    if (refused() && token() === null) setToken(takeToken());
  });
  const ctx = (t: string): AdminContext => ({
    token: t,
    m,
    pub,
    refused: () => {
      forgetToken();
      setToken(null);
      setRefused(true);
    },
  });

  return (
    <div class="app admin">
      <a class="skip-link" href="#main">
        {pub.skipToContent}
      </a>
      <header class="topbar">
        <div class="topbar-inner">
          <h1 class="brand">
            <a href={href({ page: "admin", view: { view: "calls", filter: {} } })}>
              <BotAvatar />
              <span class="brand-name">{m.title}</span>
            </a>
          </h1>
          <div class="topbar-end">
            <Show when={token()}>
              <nav class="tabs" aria-label={m.navLabel}>
                <For each={TABS}>
                  {(tab) => (
                    <a
                      href={href({ page: "admin", view: tab.view })}
                      aria-current={tab.on.includes(view().view) ? "page" : undefined}
                    >
                      {tab.label(m)}
                    </a>
                  )}
                </For>
              </nav>
            </Show>
            <ThemeSelect m={pub} />
          </div>
        </div>
      </header>
      <main class="main" id="main" tabIndex={-1}>
        <Show when={token()} fallback={<NoToken m={m} pub={pub} refused={refused()} />}>
          {(t) => (
            <Switch>
              <Match
                when={(() => {
                  const v = view();
                  return v.view === "calls" ? v : false;
                })()}
              >
                {(v) => <CallsPage ctx={ctx(t())} filter={v().filter} />}
              </Match>
              <Match
                when={(() => {
                  const v = view();
                  return v.view === "call" ? v : false;
                })()}
              >
                {(v) => <CallPage ctx={ctx(t())} id={v().id} />}
              </Match>
              <Match
                when={(() => {
                  const v = view();
                  return v.view === "feedback" ? v : false;
                })()}
              >
                {(v) => <FeedbackPage ctx={ctx(t())} filter={v().filter} />}
              </Match>
              <Match
                when={(() => {
                  const v = view();
                  return v.view === "item" ? v : false;
                })()}
              >
                {(v) => <ItemPage ctx={ctx(t())} id={v().id} />}
              </Match>
              <Match when={view().view === "failures"}>
                <FailuresPage ctx={ctx(t())} />
              </Match>
              <Match
                when={(() => {
                  const v = view();
                  return v.view === "usage" ? v : false;
                })()}
              >
                {(v) => <UsagePage ctx={ctx(t())} days={v().days ?? 30} />}
              </Match>
              <Match when={view().view === "missing"}>
                <section class="page">
                  <BotMessage m={pub}>
                    <Notice tone="note" title={m.missingTitle}>
                      <p>{m.pageMissing}</p>
                    </Notice>
                  </BotMessage>
                </section>
              </Match>
            </Switch>
          )}
        </Show>
      </main>
    </div>
  );
}

/** No token, or one the server refused: how to get the address with one. */
function NoToken(props: { m: AdminMessages; pub: Messages; refused: boolean }) {
  return (
    <section class="page">
      <BotMessage m={props.pub}>
        <Notice
          tone={props.refused ? "caution" : "note"}
          title={props.refused ? props.m.badTokenTitle : props.m.noTokenTitle}
        >
          <p>{props.refused ? props.m.badToken : props.m.noToken}</p>
          <pre class="admin-command">
            <code>{props.m.adminUrlCommand}</code>
          </pre>
        </Notice>
      </BotMessage>
    </section>
  );
}

/** A read that failed, as the judge's notice; a refused token goes back to
 * the token screen instead. */
export function ReadFailure(props: { ctx: AdminContext; read: Read<unknown> }) {
  createEffect(() => {
    if (!props.read.ok && props.read.status === 401) props.ctx.refused();
  });
  return (
    <Show when={!props.read.ok && props.read.status !== 401 && props.read}>
      {(r) => {
        const failure = r() as Extract<Read<unknown>, { ok: false }>;
        return (
          <BotMessage m={props.ctx.pub}>
            <Notice
              tone={failure.status === 503 ? "note" : "danger"}
              title={failure.status === 503 ? props.ctx.m.maintenanceTitle : props.ctx.m.errorTitle}
              role="alert"
            >
              <p>{failure.message}</p>
            </Notice>
          </BotMessage>
        );
      }}
    </Show>
  );
}
