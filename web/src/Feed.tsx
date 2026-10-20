import { createMemo, createResource, createSignal, For, Match, Show, Switch } from "solid-js";
import Answer from "./Answer";
import { type FeedEntry, type FeedQuery, fetchCall, fetchFeed, type Interface } from "./api";
import { BotMessage, Message, Notice, PersonAvatar, Typing } from "./Chat";
import { feedView, type View, ViewSwitch } from "./Controls";
import type { Locale, Messages } from "./messages";
import { RateAnswer, RatingsLine, Reasons, withWebRating } from "./Rating";
import { href, navigate, visit } from "./router";
import Symbols from "./Symbols";

/** A Discord username written with its `@`: the search box's way to ask
 * for one asker's calls rather than for text. */
const USERNAME = /^@((?!.*\.\.)[a-z0-9_.]{2,32})$/;

/** The longest search the server takes (`feed::MAX_SEARCH_CHARS`). */
const MAX_SEARCH = 100;

/** The feed's search: text in the question, the answer or the asker's
 * name, or `@name` for one Discord user's calls. Sent on Enter, kept in the
 * address, so a search is a link like any other feed. */
function SearchBox(props: { m: Messages; query: FeedQuery }) {
  let input: HTMLInputElement | undefined;
  const shown = () => (props.query.user ? `@${props.query.user}` : (props.query.q ?? ""));
  return (
    <search>
      <form
        class="search"
        onSubmit={(e) => {
          e.preventDefault();
          const text = (input?.value ?? "").trim();
          const user = USERNAME.exec(text.toLowerCase())?.[1];
          navigate(
            href({
              page: "feed",
              interface: props.query.interface,
              user,
              // By character, so a cut never splits an emoji in two.
              q:
                user === undefined && text !== ""
                  ? Array.from(text).slice(0, MAX_SEARCH).join("")
                  : undefined,
            }),
          );
        }}
      >
        <label class="sr-only" for="feed-search">
          {props.m.searchLabel}
        </label>
        <svg class="search-icon" viewBox="0 0 20 20" width="18" height="18" aria-hidden="true">
          <circle cx="8.5" cy="8.5" r="5.5" fill="none" stroke="currentColor" stroke-width="1.8" />
          <path
            d="m13 13 4.5 4.5"
            stroke="currentColor"
            stroke-width="1.8"
            stroke-linecap="round"
          />
        </svg>
        <input
          id="feed-search"
          ref={input}
          type="search"
          value={shown()}
          placeholder={props.m.searchPlaceholder}
          maxLength={MAX_SEARCH}
          autocomplete="off"
          enterkeyhint="search"
        />
        <button type="submit" class="chip">
          {props.m.searchButton}
        </button>
      </form>
    </search>
  );
}

const FILTERS: readonly Interface[] = ["discord", "web", "mcp", "cli", "agent_session"];

/** The local calendar day of an RFC 3339 time, for grouping entries. */
function day(iso: string): string {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toDateString();
}

/** A line across the timeline naming the day of the entries under it. */
export function DayDivider(props: { m: Messages; iso: string }) {
  return (
    <p class="day-divider">
      <span>{props.m.dayLabel(props.iso)}</span>
    </p>
  );
}

/** Who asked: a link to their page, or "Anonymous". */
function Asker(props: { m: Messages; entry: FeedEntry }) {
  return (
    <Show when={props.entry.asker} fallback={props.m.anonymous}>
      {(name) => <a href={href({ page: "user", name: name() })}>@{name()}</a>}
    </Show>
  );
}

/** Where and when: the interface's badge and the time, linking to the call's page. */
function AskedVia(props: { m: Messages; entry: FeedEntry }) {
  return (
    <>
      <span class="badge">{props.m.interfaceName[props.entry.interface]}</span>
      <a
        class="msg-time"
        href={href({ page: "call", id: props.entry.id })}
        title={props.m.askedAt(props.entry.asked_at)}
      >
        <time datetime={props.entry.asked_at}>{props.m.askedTime(props.entry.asked_at)}</time>
      </a>
    </>
  );
}

/** The judge's side of a call: the answer, its ratings (and the buttons to
 * rate it) and whether it is retired. */
function JudgeReply(props: { m: Messages; entry: FeedEntry; clamp?: boolean }) {
  // The counts as this page last knew them: a rating given here adds to
  // the web's.
  const [ratings, setRatings] = createSignal(props.entry.ratings);
  return (
    <BotMessage m={props.m}>
      <Answer m={props.m} answer={props.entry} clamp={props.clamp} />
      <p class="msg-meta ratings">
        <svg viewBox="0 0 16 16" width="13" height="13" aria-hidden="true">
          <path
            d="M8 1.8 9.9 5.7l4.2.6-3 3 .7 4.2L8 11.5l-3.8 2 .7-4.2-3-3 4.2-.6z"
            fill="none"
            stroke="currentColor"
            stroke-width="1.4"
            stroke-linejoin="round"
          />
        </svg>
        <RatingsLine m={props.m} ratings={ratings()} />
      </p>
      <Show when={props.entry.reasons}>
        {(reasons) => <Reasons m={props.m} reasons={reasons()} />}
      </Show>
      <RateAnswer
        m={props.m}
        call={props.entry.id}
        onRated={(score, replaced) => setRatings((r) => withWebRating(r, score, replaced))}
      />
      <Show when={props.entry.retired}>
        {(reason) => <p class="retired">{props.m.retiredNote(reason())}</p>}
      </Show>
    </BotMessage>
  );
}

/** One public call as an exchange: the asker's question (who, where, when),
 * then the judge's reply. */
export function Entry(props: { m: Messages; entry: FeedEntry; clamp?: boolean }) {
  return (
    <article class="exchange">
      <Message
        class="msg-asker"
        avatar={<PersonAvatar name={props.entry.asker} />}
        name={<Asker m={props.m} entry={props.entry} />}
        header={<AskedVia m={props.m} entry={props.entry} />}
      >
        <p class="bubble bubble-asker">
          <Symbols text={props.entry.question} />
        </p>
      </Message>
      <JudgeReply m={props.m} entry={props.entry} clamp={props.clamp} />
    </article>
  );
}

/** One call as a line: who, the question, where, when and how sure. It opens
 * in place onto the judge's reply. */
function Row(props: { m: Messages; entry: FeedEntry }) {
  return (
    <details class="row">
      <summary class="row-line">
        <PersonAvatar name={props.entry.asker} />
        <span class="row-question">
          <Symbols text={props.entry.question} />
        </span>
        <span class="row-facts">
          <span class="badge">{props.m.interfaceName[props.entry.interface]}</span>
          <time datetime={props.entry.asked_at}>{props.m.askedTime(props.entry.asked_at)}</time>
          <span class={`row-confidence confidence-text-${props.entry.confidence}`}>
            <span class={`confidence confidence-${props.entry.confidence}`} aria-hidden="true" />
            {props.m.confidenceTag(props.m.confidence[props.entry.confidence])}
          </span>
          <Show when={props.entry.retired}>
            <span class="tag-retired">{props.m.retiredBadge}</span>
          </Show>
        </span>
        <svg
          class="chevron row-chevron"
          viewBox="0 0 16 16"
          width="14"
          height="14"
          aria-hidden="true"
        >
          <path
            d="m4 6 4 4 4-4"
            fill="none"
            stroke="currentColor"
            stroke-width="1.8"
            stroke-linecap="round"
            stroke-linejoin="round"
          />
        </svg>
      </summary>
      <div class="row-body">
        <p class="row-meta">
          <span class="msg-name">
            <Asker m={props.m} entry={props.entry} />
          </span>
          <a class="msg-time" href={href({ page: "call", id: props.entry.id })}>
            <time datetime={props.entry.asked_at}>{props.m.askedAt(props.entry.asked_at)}</time>
          </a>
        </p>
        <JudgeReply m={props.m} entry={props.entry} />
      </div>
    </details>
  );
}

/** Entries under a divider for each day, as lines or as full exchanges. */
function Timeline(props: { m: Messages; entries: FeedEntry[]; view: View }) {
  // Keyed by the day's name (a string), so a group already shown keeps its
  // rows, and whatever the visitor opened in them, when an older page is
  // appended; only its entry list grows.
  const groups = createMemo(() => {
    const out = new Map<string, FeedEntry[]>();
    for (const entry of props.entries) {
      const key = day(entry.asked_at);
      const list = out.get(key);
      if (list) list.push(entry);
      else out.set(key, [entry]);
    }
    return out;
  });
  const keys = () => [...groups().keys()];
  return (
    <For each={keys()}>
      {(key) => {
        const group = {
          get entries() {
            return groups().get(key) ?? [];
          },
          get iso() {
            return groups().get(key)?.[0]?.asked_at ?? "";
          },
        };
        return (
          <>
            <DayDivider m={props.m} iso={group.iso} />
            <Show
              when={props.view === "compact"}
              fallback={
                <For each={group.entries}>
                  {(entry) => <Entry m={props.m} entry={entry} clamp={true} />}
                </For>
              }
            >
              <ul class="rows">
                <For each={group.entries}>
                  {(entry) => (
                    <li>
                      <Row m={props.m} entry={entry} />
                    </li>
                  )}
                </For>
              </ul>
            </Show>
          </>
        );
      }}
    </For>
  );
}

/** The feed, or one user's part of it: a page at a time, older pages on request. */
export function FeedPage(props: {
  m: Messages;
  locale: Locale | undefined;
  query: FeedQuery;
  title: string;
  filters: boolean;
  /** The asker whose page this is, for the heading's avatar. */
  user?: string;
  /** Whether the dashboard is served, so a user's page links to their
   * figures there. */
  dashboard?: boolean;
}) {
  const [entries, setEntries] = createSignal<FeedEntry[]>([]);
  const [next, setNext] = createSignal<string | null>(null);
  const [failure, setFailure] = createSignal<string | null>(null);
  const [loading, setLoading] = createSignal(false);
  // Each request's number: a reply to anything but the latest is dropped,
  // so an older page or another filter's page never lands on this one.
  let latest = 0;

  async function load(cursor?: string) {
    const mine = ++latest;
    if (cursor === undefined) {
      // A new query starts empty rather than under the last one's entries.
      setEntries([]);
      setNext(null);
      setFailure(null);
    }
    setLoading(true);
    const page = await fetchFeed({ ...props.query, cursor }, props.locale, props.m);
    if (mine !== latest) return;
    setLoading(false);
    if (page.ok) {
      setEntries((had) =>
        cursor === undefined ? page.value.entries : [...had, ...page.value.entries],
      );
      setNext(page.value.next);
      setFailure(null);
    } else {
      setFailure(page.message);
    }
  }
  // Each query is its own feed: start over when it changes.
  const [first] = createResource(
    () => ({ ...props.query, visit: visit() }),
    () => load(),
  );

  return (
    <section class="page">
      <header class="page-head">
        <Show when={props.user}>{(name) => <PersonAvatar name={name()} large={true} />}</Show>
        <div>
          <h2>{props.title}</h2>
          <Show when={props.filters}>
            <p class="page-intro">{props.m.feedIntro}</p>
          </Show>
          <Show when={props.user !== undefined && props.dashboard}>
            <p class="page-intro quick-links">
              <a href={href({ page: "dashboard", user: props.user })}>{props.m.userStatsLink}</a>
            </p>
          </Show>
        </div>
      </header>
      <Show when={props.filters}>
        <SearchBox m={props.m} query={props.query} />
      </Show>
      <div class="toolbar">
        <Show when={props.filters}>
          <nav class="filters" aria-label={props.m.filtersLabel}>
            <a
              class="chip"
              href={href({ page: "feed", user: props.query.user, q: props.query.q })}
              aria-current={props.query.interface === undefined ? "page" : undefined}
            >
              {props.m.feedAll}
            </a>
            <For each={FILTERS}>
              {(i) => (
                <a
                  class="chip"
                  href={href({
                    page: "feed",
                    interface: i,
                    user: props.query.user,
                    q: props.query.q,
                  })}
                  aria-current={props.query.interface === i ? "page" : undefined}
                >
                  {props.m.interfaceName[i]}
                </a>
              )}
            </For>
          </nav>
        </Show>
        <Show when={entries().length > 0}>
          <ViewSwitch m={props.m} />
        </Show>
      </div>
      <Show when={!first.loading} fallback={<Typing m={props.m} loading={true} />}>
        <Show
          when={entries().length > 0}
          fallback={
            <Show when={failure() === null}>
              <BotMessage m={props.m}>
                <p class="bubble bubble-bot">
                  {props.query.q || (props.filters && props.query.user)
                    ? props.m.searchEmpty(
                        props.query.user ? `@${props.query.user}` : (props.query.q ?? ""),
                      )
                    : props.m.feedEmpty}
                </p>
              </BotMessage>
            </Show>
          }
        >
          <Timeline m={props.m} entries={entries()} view={feedView()} />
        </Show>
      </Show>
      <Show when={failure()}>
        {(message) => (
          <BotMessage m={props.m}>
            <Notice tone="danger" title={props.m.replyTitle.error} role="alert">
              <p>{message()}</p>
            </Notice>
          </BotMessage>
        )}
      </Show>
      <Show when={next()}>
        {(cursor) => (
          <p class="more">
            <button
              type="button"
              class="button-quiet"
              disabled={loading()}
              onClick={() => void load(cursor())}
            >
              {loading() ? props.m.loading : props.m.loadMore}
            </button>
          </p>
        )}
      </Show>
    </section>
  );
}

/** `/calls/<id>`: one public call, or why there is none. */
export function CallPage(props: { m: Messages; locale: Locale | undefined; id: string }) {
  const [call] = createResource(
    () => props.id,
    (id) => fetchCall(id, props.locale, props.m),
  );
  return (
    <section class="page">
      <p class="back">
        <a href={href({ page: "feed" })}>
          <span aria-hidden="true">←</span>
          {props.m.navFeed}
        </a>
      </p>
      <Switch fallback={<Typing m={props.m} loading={true} />}>
        <Match when={call()?.ok === true && call()}>
          {(r) => {
            const read = r();
            return read.ok ? (
              <>
                <DayDivider m={props.m} iso={read.value.asked_at} />
                <Entry m={props.m} entry={read.value} />
              </>
            ) : null;
          }}
        </Match>
        <Match when={call()?.ok === false && call()}>
          {(r) => {
            const read = r();
            return read.ok ? null : (
              <BotMessage m={props.m}>
                <Show
                  when={read.status === 404}
                  fallback={
                    <Notice tone="danger" title={props.m.replyTitle.error} role="alert">
                      <p>{read.message}</p>
                    </Notice>
                  }
                >
                  <Notice tone="note" title={props.m.callMissingTitle}>
                    <p>{read.message}</p>
                  </Notice>
                </Show>
              </BotMessage>
            );
          }}
        </Match>
      </Switch>
    </section>
  );
}
