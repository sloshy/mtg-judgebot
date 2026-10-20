// /admin/usage: the operator's dashboard (#28). The public dashboard's
// figures over every call, private ones included (the same sections, from
// ../Stats), then what only the operator sees: the budget, the spend per
// day (a bar chart; the hover text and the month list carry the numbers)
// and per month, the runs by outcome, model, interface and stage with their
// latency and the dearest of them, the lowest-rated calls, the feedback
// queue and the latest data refreshes. `judge-cli stats` prints the same
// report.

import { createResource, For, Match, Show, Switch } from "solid-js";
import type { Interface } from "../api";
import { Typing } from "../Chat";
import { type AdminUsageFilter, href, navigate } from "../router";
import { BarList, DashboardSections, type StatsLinks, StatsSection, StatTiles } from "../Stats";
import { type AdminContext, ReadFailure } from "./Admin";
import { fetchUsage, type RunStats, type Usage } from "./api";

/** The windows offered, in days. */
const WINDOWS = [7, 30, 90, 366] as const;
const DEFAULT_DAYS = 30;
const INTERFACES: readonly Interface[] = ["discord", "web", "mcp", "cli", "agent_session", "agent"];

/** Where the admin figures link: the admin list of calls. */
const LINKS: StatsLinks = {
  interface: (i) => href({ page: "admin", view: { view: "calls", filter: { interface: i } } }),
  user: (name) => href({ page: "admin", view: { view: "calls", filter: { asker: name } } }),
  card: (name) => href({ page: "admin", view: { view: "calls", filter: { q: name } } }),
};

export function UsagePage(props: { ctx: AdminContext; filter: AdminUsageFilter }) {
  const m = () => props.ctx.m;
  const [read] = createResource(
    () => ({ ...props.filter }),
    (f) => fetchUsage(f, props.ctx.token, m()),
  );
  const days = () =>
    props.filter.from === undefined ? (props.filter.days ?? DEFAULT_DAYS) : undefined;
  const at = (change: Partial<AdminUsageFilter>) =>
    href({ page: "admin", view: { view: "usage", filter: { ...props.filter, ...change } } });
  return (
    <section class="page">
      <header class="page-head">
        <div>
          <h2>{m().usageTitle}</h2>
          <p class="page-intro">{m().usageIntro}</p>
        </div>
      </header>
      <nav class="filters" aria-label={m().windowLabel}>
        <For each={WINDOWS}>
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
      <Filters
        ctx={props.ctx}
        filter={props.filter}
        models={(() => {
          const r = read();
          return r?.ok ? (r.value.models ?? []).map((x) => x.model) : [];
        })()}
      />
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

/** Interface, asker and model, as a form whose submission is a new
 * address; the range stays as chosen. */
function Filters(props: { ctx: AdminContext; filter: AdminUsageFilter; models: string[] }) {
  const m = () => props.ctx.m;
  let form: HTMLFormElement | undefined;
  const value = (name: string) => {
    const v = form?.elements.namedItem(name);
    return v instanceof HTMLInputElement || v instanceof HTMLSelectElement ? v.value.trim() : "";
  };
  const models = () => {
    const chosen = props.filter.model;
    return chosen && !props.models.includes(chosen) ? [...props.models, chosen] : props.models;
  };
  return (
    <form
      ref={form}
      class="admin-filters"
      onSubmit={(e) => {
        e.preventDefault();
        const asked = value("interface");
        navigate(
          href({
            page: "admin",
            view: {
              view: "usage",
              filter: {
                days: props.filter.days,
                from: props.filter.from,
                to: props.filter.to,
                interface: INTERFACES.find((i) => i === asked),
                asker: value("asker") || undefined,
                model: value("model") || undefined,
              },
            },
          }),
        );
      }}
    >
      <label class="admin-field">
        <span>{m().filterInterface}</span>
        <select name="interface" value={props.filter.interface ?? ""}>
          <option value="">{m().any}</option>
          <For each={INTERFACES}>{(i) => <option value={i}>{m().interfaceName[i]}</option>}</For>
        </select>
      </label>
      <label class="admin-field">
        <span>{m().filterAsker}</span>
        <input
          name="asker"
          type="text"
          value={props.filter.asker ?? ""}
          placeholder={m().filterAskerPlaceholder}
          maxLength={100}
          autocomplete="off"
        />
      </label>
      <label class="admin-field">
        <span>{m().filterModel}</span>
        <select name="model" value={props.filter.model ?? ""}>
          <option value="">{m().any}</option>
          <For each={models()}>{(name) => <option value={name}>{name}</option>}</For>
        </select>
      </label>
      <p class="admin-actions">
        <button type="submit" class="chip chip-action">
          {m().applyFilters}
        </button>
        <a
          class="chip"
          href={href({
            page: "admin",
            view: {
              view: "usage",
              filter: { days: props.filter.days, from: props.filter.from, to: props.filter.to },
            },
          })}
        >
          {m().clearFilters}
        </a>
      </p>
    </form>
  );
}

function Report(props: { ctx: AdminContext; usage: Usage }) {
  const m = () => props.ctx.m;
  const pub = () => props.ctx.pub;
  const u = () => props.usage;
  return (
    <>
      <p class="stats-range">{pub().rangeLine(u().from, u().to)}</p>
      <StatTiles
        items={[
          { label: m().statQuestions, value: pub().count(u().questions) },
          { label: m().statPrivate, value: pub().count(u().private) },
          {
            label: m().statFailed,
            value: (
              <a href={href({ page: "admin", view: { view: "failures" } })}>
                {pub().count(u().failed_calls)}
              </a>
            ),
          },
          { label: m().statSpend, value: m().usd(u().usd, false) },
          ...(u().run_stats
            ? [
                { label: m().statRuns, value: pub().count(u().run_stats?.runs ?? 0) },
                {
                  label: m().statPerAnswer,
                  value:
                    u().run_stats?.usd_per_answer == null
                      ? "–"
                      : m().usd(u().run_stats?.usd_per_answer ?? 0, false),
                },
              ]
            : []),
        ]}
      />

      <StatsSection id="budget-title" title={m().budgetTitle}>
        <Show when={u().budget.period === null}>
          <p class="admin-note">{m().budgetUnknown}</p>
        </Show>
        <dl class="admin-facts">
          <Show when={u().budget.period}>
            {(period) => (
              <>
                <dt>{m().budgetPeriod}</dt>
                <dd>{m().periodName[period()]}</dd>
              </>
            )}
          </Show>
          <Show when={u().budget.cap_usd !== null}>
            <dt>{m().budgetCap}</dt>
            <dd>{m().usd(u().budget.cap_usd ?? 0, false)}</dd>
          </Show>
          <Show when={u().budget.period_start}>
            {(start) => (
              <>
                <dt>{m().budgetStart}</dt>
                <dd>{m().day(start())}</dd>
              </>
            )}
          </Show>
          <Show when={u().budget.spent_usd !== null}>
            <dt>{m().budgetSpent}</dt>
            <dd>{m().usd(u().budget.spent_usd ?? 0, false)}</dd>
            <dt>{m().budgetLeft}</dt>
            <dd>{m().usd(u().budget.left_usd ?? 0, false)}</dd>
          </Show>
        </dl>
        <Show when={u().budget.period === "process"}>
          <p class="admin-note">{m().budgetProcess}</p>
        </Show>
      </StatsSection>

      <StatsSection id="spend-title" title={m().chartTitle}>
        <SpendChart ctx={props.ctx} usage={u()} />
        <Show when={u().spend_by_month.length > 0}>
          <h4 class="stats-subhead">{m().byMonthTitle}</h4>
          <ul class="admin-list">
            <For each={u().spend_by_month}>
              {(mo) => (
                <li>
                  <strong>{m().month(mo.month)}</strong>
                  <span>{m().monthLine(m().usd(mo.usd, false), mo.llm_calls)}</span>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </StatsSection>

      <DashboardSections m={pub()} d={u()} links={LINKS} />

      <StatsSection id="runs-title" title={m().usageRunsTitle}>
        <Show when={u().run_stats} fallback={<p class="admin-note">{m().runsPerAsker}</p>}>
          {(runs) => <Runs ctx={props.ctx} runs={runs()} />}
        </Show>
      </StatsSection>

      <StatsSection id="lowest-title" title={m().lowestTitle}>
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
        <p class="admin-note">{m().retiredCalls(u().retired_calls)}</p>
      </StatsSection>

      <StatsSection id="queue-title" title={m().queueCountsTitle}>
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
      </StatsSection>

      <StatsSection id="refresh-title" title={m().refreshTitle}>
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
      </StatsSection>
    </>
  );
}

/** The runs: by outcome, model, interface and stage, how long the answered
 * ones took, and the dearest. */
function Runs(props: { ctx: AdminContext; runs: RunStats }) {
  const m = () => props.ctx.m;
  const pub = () => props.ctx.pub;
  const r = () => props.runs;
  const stageName = (stage: string) => {
    const names = m().stageName;
    return stage in names ? names[stage as keyof typeof names] : stage;
  };
  return (
    <>
      <StatTiles
        items={[
          { label: m().statRuns, value: pub().count(r().runs) },
          { label: m().statRunSpend, value: m().usd(r().usd, r().estimated > 0) },
        ]}
      />
      <p class="admin-note">{m().refusedAtCap(r().refused_at_cap)}</p>
      <Show when={r().latency}>
        {(l) => <p class="admin-note">{`${m().latencyTitle}: ${m().latencyLine(l())}`}</p>}
      </Show>

      <h4 class="stats-subhead">{m().outcomesTitle}</h4>
      <BarList
        m={pub()}
        items={r().outcomes.map((o) => ({
          key: o.outcome,
          label: o.outcome.replaceAll("_", " "),
          n: o.runs,
        }))}
      />

      <h4 class="stats-subhead">{m().byModelTitle}</h4>
      <Show when={r().by_model.length > 0} fallback={<p class="admin-empty">{m().nothingYet}</p>}>
        <ul class="admin-list">
          <For each={r().by_model}>
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

      <h4 class="stats-subhead">{m().byInterfaceTitle}</h4>
      <Show
        when={r().by_interface.length > 0}
        fallback={<p class="admin-empty">{m().nothingYet}</p>}
      >
        <ul class="admin-list">
          <For each={r().by_interface}>
            {(i) => (
              <li>
                <strong>{m().interfaceName[i.interface as Interface] ?? i.interface}</strong>
                <span>{m().spendLine(i.runs, "runs", m().usd(i.usd, false))}</span>
              </li>
            )}
          </For>
        </ul>
      </Show>

      <h4 class="stats-subhead">{m().byStageTitle}</h4>
      <Show when={r().by_stage.length > 0} fallback={<p class="admin-empty">{m().nothingYet}</p>}>
        <ul class="admin-list">
          <For each={r().by_stage}>
            {(s) => (
              <li>
                <strong>{stageName(s.stage)}</strong>
                <span>{m().spendLine(s.requests, "requests", m().usd(s.usd, false))}</span>
              </li>
            )}
          </For>
        </ul>
      </Show>

      <h4 class="stats-subhead">{m().expensiveTitle}</h4>
      <Show
        when={r().most_expensive.length > 0}
        fallback={<p class="admin-empty">{m().nothingYet}</p>}
      >
        <ul class="admin-list">
          <For each={r().most_expensive}>
            {(e) => (
              <li>
                <Show when={e.call} fallback={<span>{e.question ?? m().noQuestion}</span>}>
                  {(call) => (
                    <a href={href({ page: "admin", view: { view: "call", id: call() } })}>
                      {e.question ?? m().noQuestion}
                    </a>
                  )}
                </Show>
                <span>
                  {m().expensiveLine(
                    m().interfaceName[e.interface as Interface] ?? e.interface,
                    e.outcome,
                    m().when(e.at),
                  )}{" "}
                  · <strong>{m().usd(e.usd, e.estimated)}</strong>
                </span>
              </li>
            )}
          </For>
        </ul>
      </Show>
    </>
  );
}

/** Chart geometry, in the SVG's own units. */
const WIDTH = 640;
const HEIGHT = 160;
const AXIS = 20;

/** Spend per day as bars, oldest on the left: one series, so no legend (the
 * section's heading names it). Each bar's hover text gives the day, the
 * amount and the model calls, and the month list under it the totals. */
function SpendChart(props: { ctx: AdminContext; usage: Usage }) {
  const m = () => props.ctx.m;
  /** Every day of the range, oldest first, a day with nothing as 0. */
  const days = () => {
    const spent = new Map(props.usage.spend_by_day.map((d) => [d.day, d]));
    const out: { day: string; usd: number; calls: number }[] = [];
    const first = new Date(`${props.usage.from}T00:00:00Z`);
    for (let i = 0; i < props.usage.days; i++) {
      const key = new Date(first.getTime() + i * 86_400_000).toISOString().slice(0, 10);
      const d = spent.get(key);
      out.push({ day: key, usd: d?.usd ?? 0, calls: d?.llm_calls ?? 0 });
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
                <title>{m().chartBar(m().day(d.day), m().usd(d.usd, false), d.calls)}</title>
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
