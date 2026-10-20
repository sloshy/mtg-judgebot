// /dashboard: what the judge has been asked and how it went (#28), over the
// public questions only (GET /api/stats). The range and the filters are the
// address, so a view is a link like any other; one user's figures link to
// their answers, and the figures link into the feed.

import { createResource, For, Match, Show, Switch } from "solid-js";
import { type Dashboard, fetchStats, type Interface } from "./api";
import { BotMessage, Notice, PersonAvatar, Typing } from "./Chat";
import type { Locale, Messages } from "./messages";
import { href, type Route, visit } from "./router";
import { answeredShare, DashboardSections, type StatsLinks, StatTiles } from "./Stats";

type DashboardRoute = Extract<Route, { page: "dashboard" }>;

/** The ranges offered, in days. */
export const RANGES = [7, 30, 90, 365] as const;
/** The range a report covers when the address names none. */
export const DEFAULT_DAYS = 30;

const FILTERS: readonly Interface[] = ["discord", "web", "mcp", "cli", "agent_session"];

/** Where the public figures link: the feed. */
const LINKS: StatsLinks = {
  interface: (i) => href({ page: "feed", interface: i }),
  user: (name) => href({ page: "user", name }),
  card: (name) => href({ page: "feed", q: name }),
};

export function DashboardPage(props: {
  m: Messages;
  locale: Locale | undefined;
  route: DashboardRoute;
  /** Whether the server shows models, and so takes a model filter. */
  models: boolean;
}) {
  const m = () => props.m;
  const [read] = createResource(
    () => ({ ...props.route, visit: visit() }),
    (r) =>
      fetchStats(
        {
          days: r.from === undefined ? (r.days ?? DEFAULT_DAYS) : undefined,
          from: r.from,
          to: r.to,
          interface: r.interface,
          user: r.user,
          model: props.models ? r.model : undefined,
        },
        props.locale,
        m(),
      ),
  );
  const at = (change: Partial<DashboardRoute>): string => href({ ...props.route, ...change });
  const days = () =>
    props.route.from === undefined ? (props.route.days ?? DEFAULT_DAYS) : undefined;
  return (
    <section class="page dashboard">
      <header class="page-head">
        <Show when={props.route.user}>{(name) => <PersonAvatar name={name()} large={true} />}</Show>
        <div>
          <h2>
            {props.route.user ? m().dashboardUserTitle(props.route.user) : m().dashboardTitle}
          </h2>
          <Show when={props.route.user} fallback={<p class="page-intro">{m().dashboardIntro}</p>}>
            {(name) => (
              <p class="page-intro quick-links">
                <a href={href({ page: "user", name: name() })}>{m().userAnswers(name())}</a>
                <a href={at({ user: undefined })}>{m().everyone}</a>
              </p>
            )}
          </Show>
        </div>
      </header>

      <div class="stats-filters">
        <nav class="filters" aria-label={m().rangeLabel}>
          <For each={RANGES}>
            {(n) => (
              <a
                class="chip"
                href={at({
                  days: n === DEFAULT_DAYS ? undefined : n,
                  from: undefined,
                  to: undefined,
                })}
                aria-current={days() === n ? "page" : undefined}
              >
                {m().lastDays(n)}
              </a>
            )}
          </For>
        </nav>
        <nav class="filters" aria-label={m().filtersLabel}>
          <a
            class="chip"
            href={at({ interface: undefined })}
            aria-current={props.route.interface === undefined ? "page" : undefined}
          >
            {m().feedAll}
          </a>
          <For each={FILTERS}>
            {(i) => (
              <a
                class="chip"
                href={at({ interface: i })}
                aria-current={props.route.interface === i ? "page" : undefined}
              >
                {m().interfaceName[i]}
              </a>
            )}
          </For>
        </nav>
        <Show when={props.models && read()?.ok === true && read()}>
          {(r) => {
            const got = r();
            const names = () => {
              const listed = got.ok ? (got.value.models ?? []).map((x) => x.model) : [];
              const chosen = props.route.model;
              return chosen && !listed.includes(chosen) ? [...listed, chosen] : listed;
            };
            return (
              <Show when={names().length > 0}>
                <nav class="filters" aria-label={m().modelFilterLabel}>
                  <a
                    class="chip"
                    href={at({ model: undefined })}
                    aria-current={props.route.model === undefined ? "page" : undefined}
                  >
                    {m().allModels}
                  </a>
                  <For each={names()}>
                    {(name) => (
                      <a
                        class="chip chip-mono"
                        href={at({ model: name })}
                        aria-current={props.route.model === name ? "page" : undefined}
                      >
                        {name}
                      </a>
                    )}
                  </For>
                </nav>
              </Show>
            );
          }}
        </Show>
      </div>

      <Switch fallback={<Typing m={m()} loading={true} />}>
        <Match when={read()?.ok === true && read()}>
          {(r) => {
            const got = r();
            return got.ok ? <Report m={m()} d={got.value} /> : null;
          }}
        </Match>
        <Match when={read()?.ok === false && read()}>
          {(r) => {
            const failed = r();
            return failed.ok ? null : (
              <BotMessage m={m()}>
                <Notice
                  tone={failed.status === 503 ? "note" : "danger"}
                  title={
                    failed.status === 503
                      ? m().replyTitle.maintenance
                      : failed.status === 429
                        ? m().replyTitle.rate_limited
                        : m().replyTitle.error
                  }
                  role="alert"
                >
                  <p>{failed.message}</p>
                </Notice>
              </BotMessage>
            );
          }}
        </Match>
      </Switch>
    </section>
  );
}

function Report(props: { m: Messages; d: Dashboard }) {
  const m = () => props.m;
  return (
    <>
      <p class="stats-range">
        {m().rangeLine(props.d.from, props.d.to)}
        <Show when={props.d.window_days}>
          {(n) => <span class="stats-window"> {m().windowNote(n())}</span>}
        </Show>
      </p>
      <StatTiles
        items={[
          { label: m().statQuestions, value: m().count(props.d.questions) },
          { label: m().statAskers, value: m().count(props.d.askers) },
          ...(answeredShare(m(), props.d) === null
            ? []
            : [{ label: m().statAnswered, value: answeredShare(m(), props.d) ?? "" }]),
        ]}
      />
      <Show
        when={props.d.questions > 0}
        fallback={
          <BotMessage m={m()}>
            <p class="bubble bubble-bot">{m().dashboardEmpty}</p>
          </BotMessage>
        }
      >
        <DashboardSections m={m()} d={props.d} links={LINKS} />
      </Show>
    </>
  );
}
