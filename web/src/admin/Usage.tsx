// /admin/usage: questions and spend over a window. Four headline numbers,
// the spend per day as a bar chart (the day list under it is the same data
// as text), then the runs by model and outcome, the ratings, the
// lowest-rated calls and the latest data refreshes.

import { createResource, For, Match, Show, Switch } from "solid-js";
import { Typing } from "../Chat";
import { href } from "../router";
import { type AdminContext, ReadFailure } from "./Admin";
import { type Day, fetchUsage, type Usage } from "./api";

/** The windows offered, in days. */
const WINDOWS = [7, 30, 90, 366] as const;

export function UsagePage(props: { ctx: AdminContext; days: number }) {
  const m = () => props.ctx.m;
  const [read] = createResource(
    () => props.days,
    (days) => fetchUsage(days, props.ctx.token, m()),
  );
  return (
    <section class="page">
      <header class="page-head">
        <div>
          <h2>{m().usageTitle}</h2>
        </div>
      </header>
      <nav class="filters" aria-label={m().windowLabel}>
        <For each={WINDOWS}>
          {(days) => (
            <a
              class="chip"
              href={href({ page: "admin", view: { view: "usage", days } })}
              aria-current={props.days === days ? "page" : undefined}
            >
              {m().lastDays(days)}
            </a>
          )}
        </For>
      </nav>
      <Switch fallback={<Typing m={props.ctx.pub} loading={true} />}>
        <Match when={read()?.ok === true && read()}>
          {(r) => {
            const got = r();
            return got.ok ? <Report ctx={props.ctx} usage={got.value} /> : null;
          }}
        </Match>
        <Match when={read()?.ok === false && read()}>
          {(r) => <ReadFailure ctx={props.ctx} read={r()} />}
        </Match>
      </Switch>
    </section>
  );
}

function questions(d: Day): number {
  return d.discord + d.web + d.mcp + d.cli + d.agent_session + d.agent;
}

function Report(props: { ctx: AdminContext; usage: Usage }) {
  const m = () => props.ctx.m;
  const u = () => props.usage;
  const tokens = (n: number) => new Intl.NumberFormat("en").format(n);
  return (
    <>
      <ul class="admin-stats">
        <li class="admin-stat">
          <span>{m().statQuestions}</span>
          <strong>{u().questions}</strong>
        </li>
        <li class="admin-stat">
          <span>{m().statSpend}</span>
          <strong>{m().usd(u().usd, false)}</strong>
        </li>
        <li class="admin-stat">
          <span>{m().statRuns}</span>
          <strong>{u().runs}</strong>
        </li>
        <li class="admin-stat">
          <span>{m().statRunSpend}</span>
          <strong>{m().usd(u().run_usd, false)}</strong>
        </li>
      </ul>

      <section class="admin-section" aria-labelledby="chart-title">
        <h3 id="chart-title">{m().chartTitle}</h3>
        <SpendChart ctx={props.ctx} usage={u()} />
      </section>

      <section class="admin-section" aria-labelledby="by-day-title">
        <h3 id="by-day-title">{m().byDayTitle}</h3>
        <Show when={u().by_day.length > 0} fallback={<p class="admin-empty">{m().nothingYet}</p>}>
          <ul class="admin-list">
            <For each={u().by_day}>
              {(d) => (
                <li>
                  <strong>{m().day(d.day)}</strong>
                  <span>
                    {m().dayLine(questions(d), d.private, m().usd(d.usd, false), d.llm_calls)}
                  </span>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </section>

      <section class="admin-section" aria-labelledby="by-model-title">
        <h3 id="by-model-title">{m().byModelTitle}</h3>
        <Show when={u().by_model.length > 0} fallback={<p class="admin-empty">{m().nothingYet}</p>}>
          <ul class="admin-list">
            <For each={u().by_model}>
              {(model) => (
                <li>
                  <span>
                    <strong>{model.model}</strong> · {model.provider}
                  </span>
                  <span>
                    {m().modelLine(
                      model.requests,
                      m().tokens(model.input_tokens, model.output_tokens),
                      model.estimated,
                    )}{" "}
                    · <strong>{m().usd(model.usd, model.estimated > 0)}</strong>
                  </span>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </section>

      <section class="admin-section" aria-labelledby="outcomes-title">
        <h3 id="outcomes-title">{m().outcomesTitle}</h3>
        <Show when={u().outcomes.length > 0} fallback={<p class="admin-empty">{m().nothingYet}</p>}>
          <ul class="admin-list">
            <For each={u().outcomes}>
              {(o) => (
                <li>
                  <span>{o.outcome.replaceAll("_", " ")}</span>
                  <strong>{tokens(o.runs)}</strong>
                </li>
              )}
            </For>
          </ul>
        </Show>
        <p class="admin-note">{m().ratingsSummary(u().ratings)}</p>
        <Show when={u().web_ratings}>
          {(web) => <p class="admin-note">{m().webRatingsSummary(web())}</p>}
        </Show>
        <p class="admin-note">{m().retiredCalls(u().retired_calls)}</p>
      </section>

      <section class="admin-section" aria-labelledby="lowest-title">
        <h3 id="lowest-title">{m().lowestTitle}</h3>
        <Show
          when={u().lowest_rated.length > 0}
          fallback={<p class="admin-empty">{m().nothingYet}</p>}
        >
          <ul class="admin-list">
            <For each={u().lowest_rated}>
              {(c) => (
                <li>
                  <a href={href({ page: "admin", view: { view: "call", id: c.call } })}>
                    {c.question}
                  </a>
                  <span>
                    {m().lowestLine(c.effective_score, c.ratings)}
                    <Show when={c.retired}>
                      {" "}
                      <span class="tag-retired">{m().retiredTag}</span>
                    </Show>
                  </span>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </section>

      <section class="admin-section" aria-labelledby="queue-title">
        <h3 id="queue-title">{m().queueCountsTitle}</h3>
        <Show
          when={(u().feedback ?? []).some((c) => c.open + c.resolved + c.dismissed > 0)}
          fallback={<p class="admin-empty">{m().nothingYet}</p>}
        >
          <ul class="admin-list">
            <For each={(u().feedback ?? []).filter((c) => c.open + c.resolved + c.dismissed > 0)}>
              {(c) => (
                <li>
                  <Show when={c.category} fallback={<span>{m().noCategory}</span>}>
                    {(category) => (
                      <a
                        href={href({
                          page: "admin",
                          view: { view: "feedback", filter: { category: category() } },
                        })}
                      >
                        {m().categoryName[category()]}
                      </a>
                    )}
                  </Show>
                  <span>{m().queueCountLine(c.open, c.resolved, c.dismissed)}</span>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </section>

      <section class="admin-section" aria-labelledby="refresh-title">
        <h3 id="refresh-title">{m().refreshTitle}</h3>
        <Show when={u().refresh_runs_note}>{(note) => <p class="admin-note">{note()}</p>}</Show>
        <Show
          when={u().refresh_runs.length > 0}
          fallback={<p class="admin-empty">{m().nothingYet}</p>}
        >
          <ul class="admin-list">
            <For each={u().refresh_runs}>
              {(r) => (
                <li>
                  <strong>{r.started_at}</strong>
                  <span>
                    {m().refreshLine(r)}
                    <Show when={r.failed_steps.length > 0}>
                      {" · "}
                      {m().refreshFailedSteps(r.failed_steps)}
                    </Show>
                  </span>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </section>
    </>
  );
}

/** Chart geometry, in the SVG's own units. */
const WIDTH = 640;
const HEIGHT = 160;
const AXIS = 20;

/** Spend per day as bars, oldest on the left: one series, so no legend (the
 * section's heading names it). Each bar's hover text gives the day and the
 * amount, and the day list under the chart is the same data as text. */
function SpendChart(props: { ctx: AdminContext; usage: Usage }) {
  const m = () => props.ctx.m;
  /** Every day of the window, oldest first, a day with nothing as 0. */
  const days = () => {
    const spent = new Map(props.usage.by_day.map((d) => [d.day, d.usd]));
    const out: { day: string; usd: number }[] = [];
    const today = new Date();
    for (let i = props.usage.days - 1; i >= 0; i--) {
      const d = new Date(
        Date.UTC(today.getUTCFullYear(), today.getUTCMonth(), today.getUTCDate() - i),
      );
      const key = d.toISOString().slice(0, 10);
      out.push({ day: key, usd: spent.get(key) ?? 0 });
    }
    return out;
  };
  const max = () => Math.max(...days().map((d) => d.usd), 0);
  const step = () => WIDTH / Math.max(days().length, 1);
  const bar = () => Math.max(step() - 2, 1);
  const height = (usd: number) => (max() === 0 ? 0 : ((HEIGHT - AXIS) * usd) / max());
  return (
    <figure class="admin-chart">
      <svg viewBox={`0 0 ${WIDTH} ${HEIGHT}`} role="img" aria-label={m().chartTitle}>
        <line class="admin-chart-axis" x1="0" x2={WIDTH} y1={HEIGHT - AXIS} y2={HEIGHT - AXIS} />
        <For each={days()}>
          {(d, i) => (
            <Show when={d.usd > 0}>
              <path
                class="admin-chart-bar"
                d={barPath(i() * step() + 1, HEIGHT - AXIS, bar(), Math.max(height(d.usd), 2))}
              >
                <title>{m().chartBar(m().day(d.day), m().usd(d.usd, false))}</title>
              </path>
            </Show>
          )}
        </For>
      </svg>
      <figcaption class="admin-chart-range">
        <span>{m().day(days()[0]?.day ?? "")}</span>
        <span>{m().chartMax(m().usd(max(), false))}</span>
        <span>{m().day(days()[days().length - 1]?.day ?? "")}</span>
      </figcaption>
    </figure>
  );
}

/** A bar standing on `base`, its top corners rounded (up to 4 units). */
function barPath(x: number, base: number, width: number, height: number): string {
  const r = Math.min(4, width / 2, height);
  const top = base - height;
  return [
    `M${x},${base}`,
    `V${top + r}`,
    `Q${x},${top} ${x + r},${top}`,
    `H${x + width - r}`,
    `Q${x + width},${top} ${x + width},${top + r}`,
    `V${base}`,
    "Z",
  ].join(" ");
}
