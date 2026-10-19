import { createResource, createSignal, For, Match, Show, Switch } from "solid-js";
import Answer from "./Answer";
import { type FeedEntry, type FeedQuery, fetchCall, fetchFeed, type Interface } from "./api";
import type { Locale, Messages } from "./messages";
import { href } from "./router";
import Symbols from "./Symbols";

const FILTERS: readonly Interface[] = ["discord", "web", "mcp", "cli", "agent_session"];

/** One public call: who asked where and when, the question, the answer, its ratings. */
export function Entry(props: { m: Messages; entry: FeedEntry }) {
  const asker = () =>
    props.entry.asker === null ? (
      props.m.anonymous
    ) : (
      <a href={href({ page: "user", name: props.entry.asker })}>@{props.entry.asker}</a>
    );
  return (
    <article class="exchange">
      <p class="meta">
        {props.m.entryMeta(
          asker(),
          props.m.interfaceName[props.entry.interface],
          <a href={href({ page: "call", id: props.entry.id })}>
            <time datetime={props.entry.asked_at}>{props.m.askedAt(props.entry.asked_at)}</time>
          </a>,
        )}
      </p>
      <p class="question">
        <Symbols text={props.entry.question} />
      </p>
      <Answer m={props.m} answer={props.entry} />
      <p class="meta">{props.m.ratingsLine(props.entry.ratings)}</p>
      <Show when={props.entry.retired}>
        {(reason) => <p class="hint">{props.m.retiredNote(reason())}</p>}
      </Show>
    </article>
  );
}

/** The feed, or one user's part of it: a page at a time, older pages on request. */
export function FeedPage(props: {
  m: Messages;
  locale: Locale | undefined;
  query: FeedQuery;
  title: string;
  filters: boolean;
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
    () => ({ ...props.query }),
    () => load(),
  );

  return (
    <section>
      <h2>{props.title}</h2>
      <Show when={props.filters}>
        <p class="meta">{props.m.feedIntro}</p>
        <p class="meta">
          <a href={href({ page: "feed" })}>{props.m.feedAll}</a>
          <For each={FILTERS}>
            {(i) => (
              <>
                {props.m.separator}
                <a href={href({ page: "feed", interface: i })}>{props.m.interfaceName[i]}</a>
              </>
            )}
          </For>
        </p>
      </Show>
      <Show when={!first.loading} fallback={<p class="thinking">{props.m.loading}</p>}>
        <For
          each={entries()}
          fallback={<Show when={failure() === null}>{<p>{props.m.feedEmpty}</p>}</Show>}
        >
          {(entry) => <Entry m={props.m} entry={entry} />}
        </For>
      </Show>
      <Show when={failure()}>{(message) => <p class="notice">{message()}</p>}</Show>
      <Show when={next()}>
        {(cursor) => (
          <button type="button" disabled={loading()} onClick={() => void load(cursor())}>
            {loading() ? props.m.loading : props.m.loadMore}
          </button>
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
    <Switch fallback={<p class="thinking">{props.m.loading}</p>}>
      <Match when={call()?.ok === true && call()}>
        {(r) => {
          const read = r();
          return read.ok ? <Entry m={props.m} entry={read.value} /> : null;
        }}
      </Match>
      <Match when={call()?.ok === false && call()}>
        {(r) => {
          const read = r();
          return read.ok ? null : <p class="notice">{read.message}</p>;
        }}
      </Match>
    </Switch>
  );
}
