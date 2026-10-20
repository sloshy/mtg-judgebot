// The dashboard's figures (#28), shared by the public page (/dashboard) and
// the admin page's usage view, which shows the same report over every call:
// headline numbers, questions per day by interface as a stacked bar chart
// (with a table as its text alternative), then the outcome and rating mixes
// and the most-asked categories, cards and askers as bar lists. Cost and
// models appear only when the report carries them.

import { createMemo, For, type JSX, Show } from "solid-js";
import type { Dashboard, DayCount, Interface, Outcomes, RatingCounts } from "./api";
import type { Messages } from "./messages";
import { toggleWeb, webRatingsShown } from "./Rating";

/** The interfaces in the chart's stacking order, bottom first: a fixed
 * order, so each keeps its colour whatever the filter. */
export const SERIES: readonly Interface[] = [
  "discord",
  "web",
  "mcp",
  "cli",
  "agent_session",
  "agent",
];

/** Where the figures link to: the public page's feed, or the admin list. */
export interface StatsLinks {
  /** One interface's questions. */
  interface: (i: Interface) => string;
  /** One Discord user's questions. */
  user: (name: string) => string;
  /** Questions about one card. */
  card: (name: string) => string;
}

/** Headline numbers, as a row of tiles. */
export function StatTiles(props: { items: { label: string; value: JSX.Element }[] }) {
  return (
    <ul class="stat-tiles">
      <For each={props.items}>
        {(item) => (
          <li class="stat-tile">
            <span>{item.label}</span>
            <strong>{item.value}</strong>
          </li>
        )}
      </For>
    </ul>
  );
}

/** One line of a bar list. */
export interface BarItem {
  key: string;
  label: JSX.Element;
  n: number;
  /** Beside the count, such as its share. */
  note?: string;
}

/** Counts as labelled horizontal bars, each against the largest. */
export function BarList(props: { m: Messages; items: BarItem[]; empty?: string }) {
  const max = () => Math.max(1, ...props.items.map((i) => i.n));
  return (
    <Show
      when={props.items.length > 0}
      fallback={<p class="stats-empty">{props.empty ?? props.m.nothingYet}</p>}
    >
      <ul class="bar-list">
        <For each={props.items}>
          {(item) => (
            <li>
              <span class="bar-label">{item.label}</span>
              <span class="bar-value">
                {props.m.count(item.n)}
                <Show when={item.note}>{(note) => <span class="bar-note"> · {note()}</span>}</Show>
              </span>
              <span class="bar-track" aria-hidden="true">
                <Show when={item.n > 0}>
                  <span class="bar-fill" style={{ width: `${(100 * item.n) / max()}%` }} />
                </Show>
              </span>
            </li>
          )}
        </For>
      </ul>
    </Show>
  );
}

/** A section with a heading. */
export function StatsSection(props: { id: string; title: string; children: JSX.Element }) {
  return (
    <section class="stats-section" aria-labelledby={props.id}>
      <h3 id={props.id}>{props.title}</h3>
      {props.children}
    </section>
  );
}

/** Chart geometry, in the SVG's own units. */
const WIDTH = 640;
const HEIGHT = 180;
const AXIS = 2;
/** Above this many days, a bar is a week. */
const DAILY_UP_TO = 92;

interface Bucket {
  /** Its first day. */
  day: string;
  week: boolean;
  counts: Record<Interface, number>;
  total: number;
}

function buckets(days: DayCount[]): Bucket[] {
  const week = days.length > DAILY_UP_TO;
  const size = week ? 7 : 1;
  const out: Bucket[] = [];
  for (let i = 0; i < days.length; i += size) {
    const slice = days.slice(i, i + size);
    const first = slice[0];
    if (!first) continue;
    const counts = Object.fromEntries(
      SERIES.map((s) => [s, slice.reduce((n, d) => n + (d[s] ?? 0), 0)]),
    ) as Record<Interface, number>;
    out.push({
      day: first.day,
      week,
      counts,
      total: SERIES.reduce((n, s) => n + counts[s], 0),
    });
  }
  return out;
}

/** Questions per day (per week over a long range) by interface, stacked in
 * [`SERIES`] order. Each bar's hover text gives its counts; the legend
 * names the interfaces (each a link to their questions), and the table
 * under it is the same data as text. */
export function QuestionsChart(props: { m: Messages; d: Dashboard; links: StatsLinks }) {
  const bars = createMemo(() => buckets(props.d.by_day));
  const present = createMemo(() =>
    SERIES.filter((s) => props.d.interfaces.some((i) => i.interface === s && i.questions > 0)),
  );
  const peak = () => Math.max(0, ...bars().map((b) => b.total));
  const step = () => WIDTH / Math.max(bars().length, 1);
  const width = () => Math.max(step() - (step() > 4 ? 2 : 0.5), 0.5);
  const scale = (n: number) => (peak() === 0 ? 0 : ((HEIGHT - AXIS) * n) / peak());
  const label = (b: Bucket) => (b.week ? props.m.chartWeekOf(b.day) : props.m.shortDay(b.day));
  const hover = (b: Bucket) =>
    [
      `${label(b)}: ${props.m.count(b.total)}`,
      ...SERIES.filter((s) => b.counts[s] > 0).map(
        (s) => `${props.m.interfaceName[s]} ${props.m.count(b.counts[s])}`,
      ),
    ].join("\n");
  return (
    <figure class="chart">
      <svg
        viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
        preserveAspectRatio="none"
        role="img"
        aria-label={props.m.chartSummary(
          props.m.shortDay(props.d.from),
          props.m.shortDay(props.d.to),
          props.d.questions,
          peak(),
        )}
      >
        <line class="chart-axis" x1="0" x2={WIDTH} y1={HEIGHT - 1} y2={HEIGHT - 1} />
        <For each={bars()}>
          {(b, i) => {
            const x = () => i() * step() + (step() - width()) / 2;
            // Each segment stands on the ones below it, a gap of the
            // surface between them.
            const segments = () => {
              let base = HEIGHT - AXIS;
              return SERIES.filter((s) => b.counts[s] > 0).map((s) => {
                const h = Math.max(scale(b.counts[s]), 1.5);
                const y = base - h;
                base = y;
                return { s, y, h };
              });
            };
            return (
              <Show when={b.total > 0}>
                <g class="chart-bar">
                  <title>{hover(b)}</title>
                  <rect class="chart-hit" x={i() * step()} y="0" width={step()} height={HEIGHT} />
                  <For each={segments()}>
                    {(seg, j) => (
                      <rect
                        class={`chart-seg series-${seg.s}`}
                        x={x()}
                        y={seg.y}
                        width={width()}
                        height={Math.max(seg.h - (j() === 0 ? 0 : 1), 0.75)}
                      />
                    )}
                  </For>
                </g>
              </Show>
            );
          }}
        </For>
      </svg>
      <figcaption class="chart-range">
        <span>{props.m.shortDay(props.d.from)}</span>
        <span>{props.m.chartPeak(peak())}</span>
        <span>{props.m.shortDay(props.d.to)}</span>
      </figcaption>
      <ul class="chart-legend" aria-label={props.m.interfacesTitle}>
        <For each={present()}>
          {(s) => (
            <li>
              <a href={props.links.interface(s)}>
                <span class={`chart-swatch series-${s}`} aria-hidden="true" />
                {props.m.interfaceName[s]}
                <span class="chart-legend-n">
                  {props.m.count(props.d.interfaces.find((i) => i.interface === s)?.questions ?? 0)}
                </span>
              </a>
            </li>
          )}
        </For>
      </ul>
      <details class="chart-table">
        <summary class="chart-summary">{props.m.chartTable}</summary>
        <div class="table-scroll">
          <table>
            <thead>
              <tr>
                <th scope="col">{props.m.chartDay}</th>
                <For each={present()}>{(s) => <th scope="col">{props.m.interfaceName[s]}</th>}</For>
                <th scope="col">{props.m.chartTotal}</th>
              </tr>
            </thead>
            <tbody>
              <For each={bars().filter((b) => b.total > 0)}>
                {(b) => (
                  <tr>
                    <th scope="row">{label(b)}</th>
                    <For each={present()}>{(s) => <td>{props.m.count(b.counts[s])}</td>}</For>
                    <td>{props.m.count(b.total)}</td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </details>
    </figure>
  );
}

const OUTCOMES: readonly (keyof Outcomes)[] = [
  "answered",
  "ambiguous",
  "not_found",
  "out_of_scope",
  "refused",
  "failed",
];

/** The share of runs answered, or null without outcomes. */
export function answeredShare(m: Messages, d: Dashboard): string | null {
  const o = d.outcomes;
  if (o === null) return null;
  return m.percent(
    o.answered,
    OUTCOMES.reduce((n, k) => n + o[k], 0),
  );
}

function ratingTotal(r: RatingCounts): number {
  return r.correct + r.partial + r.incorrect;
}

/** Everything the public dashboard shows after its headline numbers. */
export function DashboardSections(props: { m: Messages; d: Dashboard; links: StatsLinks }) {
  const m = () => props.m;
  const d = () => props.d;
  const outcomes = (): BarItem[] => {
    const o = d().outcomes;
    if (o === null) return [];
    const total = OUTCOMES.reduce((n, k) => n + o[k], 0);
    return OUTCOMES.filter((k) => o[k] > 0).map((k) => ({
      key: k,
      label: m().outcomeName[k],
      n: o[k],
      note: m().percent(o[k], total),
    }));
  };
  const ratings = (): BarItem[] => {
    const r = d().ratings;
    const shown: RatingCounts = webRatingsShown()
      ? {
          correct: r.discord.correct + r.web.correct,
          partial: r.discord.partial + r.web.partial,
          incorrect: r.discord.incorrect + r.web.incorrect,
        }
      : r.discord;
    const total = ratingTotal(shown);
    if (total === 0) return [];
    return (["correct", "partial", "incorrect"] as const).map((k) => ({
      key: k,
      label: m().scoreName[k],
      n: shown[k],
      note: m().percent(shown[k], total),
    }));
  };
  return (
    <>
      <StatsSection id="stats-chart" title={m().chartTitle}>
        <QuestionsChart m={m()} d={d()} links={props.links} />
      </StatsSection>

      <StatsSection id="stats-outcomes" title={m().outcomesTitle}>
        <Show
          when={d().outcomes !== null}
          fallback={<p class="stats-empty">{m().outcomesPerUser}</p>}
        >
          <BarList m={m()} items={outcomes()} />
        </Show>
      </StatsSection>

      <StatsSection id="stats-ratings" title={m().ratingsTitle}>
        <Show when={ratingTotal(d().ratings.web) > 0}>
          <label class="stats-toggle">
            <input type="checkbox" checked={webRatingsShown()} onChange={() => toggleWeb()} />
            {m().includeWebToggle(ratingTotal(d().ratings.web))}
          </label>
        </Show>
        <BarList m={m()} items={ratings()} empty={m().noRatings} />
      </StatsSection>

      <StatsSection id="stats-categories" title={m().categoriesTitle}>
        <BarList
          m={m()}
          items={d().categories.map((c) => ({ key: c.category, label: c.label, n: c.questions }))}
        />
      </StatsSection>

      <StatsSection id="stats-cards" title={m().cardsTitle}>
        <BarList
          m={m()}
          items={d().cards.map((c) => ({
            key: c.id,
            label: <a href={props.links.card(c.name)}>{c.name}</a>,
            n: c.questions,
          }))}
        />
      </StatsSection>

      <Show when={d().top_askers.length > 0}>
        <StatsSection id="stats-askers" title={m().askersTitle}>
          <BarList
            m={m()}
            items={d().top_askers.map((a) => ({
              key: a.name,
              label: <a href={props.links.user(a.name)}>@{a.name}</a>,
              n: a.questions,
            }))}
          />
        </StatsSection>
      </Show>

      <Show when={d().cost}>
        {(cost) => (
          <StatsSection id="stats-cost" title={m().costTitle}>
            <StatTiles
              items={[
                { label: m().costTotal, value: m().usd(cost().usd, cost().estimated) },
                {
                  label: m().costPerQuestion,
                  value:
                    cost().per_question_usd === null
                      ? "–"
                      : m().usd(cost().per_question_usd ?? 0, cost().estimated),
                },
              ]}
            />
          </StatsSection>
        )}
      </Show>

      <Show when={d().models}>
        {(models) => (
          <StatsSection id="stats-models" title={m().modelsTitle}>
            <BarList
              m={m()}
              items={models().map((x) => ({
                key: x.model,
                label: <code>{x.model}</code>,
                n: x.answered,
                note: m().modelRead(x.extracted),
              }))}
            />
          </StatsSection>
        )}
      </Show>
    </>
  );
}
