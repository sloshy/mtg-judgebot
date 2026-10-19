import {
  createEffect,
  createMemo,
  createResource,
  createSignal,
  For,
  Match,
  onCleanup,
  Show,
  Switch,
} from "solid-js";
import { createStore } from "solid-js/store";
import {
  type About,
  type ApiReply,
  askJudge,
  fetchAbout,
  freshnessLine,
  type Pin,
  sessionId,
} from "./api";
import Markdown from "./Markdown";
import { isLocale, type Locale, type Messages, messages, pickLocale } from "./messages";
import Symbols from "./Symbols";

const MAX_QUESTION_CHARS = 1000;

interface Entry {
  question: string;
  pins: Pin[];
  /** null while the judge is thinking. */
  reply: ApiReply | null;
}

export default function App() {
  const [entries, setEntries] = createStore<Entry[]>([]);
  const [draft, setDraft] = createSignal("");
  const [waiting, setWaiting] = createSignal(false);
  let box: HTMLTextAreaElement | undefined;
  const session = sessionId();
  // `null` while GET /api/about is pending or after it failed; the footer
  // then says so rather than naming a repository this instance may not be.
  // When /api/about answered, and a clock ticking each minute, so the
  // footer's "refreshed … ago" ages with the page instead of freezing.
  let aboutAt = Date.now();
  const [about] = createResource(async () => {
    const a = await fetchAbout();
    aboutAt = Date.now();
    return a;
  });
  const [now, setNow] = createSignal(Date.now());
  const tick = setInterval(() => setNow(Date.now()), 60_000);
  onCleanup(() => clearInterval(tick));
  // The visitor's first browser language we have a catalog for; failing
  // that, the instance's default once /api/about names one we have. Only
  // a browser match is sent with a question, so one asked before
  // /api/about answers still gets the instance's default from the server.
  const browserLocale = pickLocale(navigator.languages, undefined);
  const locale = createMemo<Locale>(() => {
    const instance = about()?.locale;
    return browserLocale ?? (isLocale(instance) ? instance : "en");
  });
  const m = createMemo(() => messages(locale()));
  createEffect(() => {
    document.documentElement.lang = locale();
  });

  async function ask(question: string, pins: Pin[], index?: number) {
    const i = index ?? entries.length;
    if (index === undefined) {
      setEntries(i, { question, pins, reply: null });
    } else {
      setEntries(i, { pins, reply: null });
    }
    setWaiting(true);
    try {
      const reply = await askJudge(question, pins, session, browserLocale, m());
      setEntries(i, "reply", reply);
    } finally {
      setWaiting(false);
    }
  }

  function submit(e: SubmitEvent) {
    e.preventDefault();
    const question = draft().trim();
    if (!question || waiting()) return;
    setDraft("");
    void ask(question, []);
  }

  /** Choosing an example fills the box rather than asking: an answer costs
   * the operator money and the visitor one of their rate-limited questions,
   * so sending it stays their decision. */
  function tryExample(question: string) {
    // Never over something the visitor typed; one example may replace another.
    const typed = draft().trim();
    if (!typed || m().examples.some((e) => e === typed)) setDraft(question);
    box?.focus();
  }

  /** Ask an entry again as it was, after a maintenance reply. */
  function retry(index: number) {
    const entry = entries[index];
    if (!entry || waiting()) return;
    void ask(entry.question, entry.pins, index);
  }

  /** A "did you mean…?" pick: re-ask the same entry with the span pinned. */
  function pick(index: number, span: string, name: string) {
    const entry = entries[index];
    if (!entry || waiting()) return;
    void ask(entry.question, [...entry.pins, { span, name }], index);
  }

  return (
    <main class="app">
      <header>
        <h1>
          <img src="/favicon.png" alt="" width="32" height="32" />
          {m().title}
        </h1>
        <p class="tagline">{m().tagline}</p>
      </header>

      <For each={entries}>
        {(entry, i) => (
          <section class="exchange">
            <p class="question">
              <Symbols text={entry.question} />
            </p>
            <Show when={entry.reply} fallback={<p class="thinking">{m().thinking}</p>}>
              {(reply) => (
                <Reply
                  m={m()}
                  reply={reply()}
                  onPick={(span, name) => pick(i(), span, name)}
                  onRetry={() => retry(i())}
                  waiting={waiting()}
                />
              )}
            </Show>
          </section>
        )}
      </For>

      <Show when={entries.length === 0}>
        <section class="examples" aria-labelledby="examples-title">
          <h2 id="examples-title">{m().examplesTitle}</h2>
          <ul>
            <For each={m().examples}>
              {(question) => (
                <li>
                  <button type="button" onClick={() => tryExample(question)}>
                    <Symbols text={question} />
                  </button>
                </li>
              )}
            </For>
          </ul>
        </section>
      </Show>

      <form class="ask" onSubmit={submit}>
        <textarea
          ref={box}
          value={draft()}
          onInput={(e) => setDraft(e.currentTarget.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              e.currentTarget.form?.requestSubmit();
            }
          }}
          maxLength={MAX_QUESTION_CHARS}
          rows="3"
          placeholder={m().placeholder}
          disabled={waiting()}
        />
        <button type="submit" disabled={waiting() || !draft().trim()}>
          {waiting() ? m().judging : m().ask}
        </button>
      </form>

      <footer>
        <p>{m().notRated}</p>
        <p>
          {m().fanContent((label) => (
            <a href="https://company.wizards.com/en/legal/fancontentpolicy">{label}</a>
          ))}
        </p>
        <p>
          {m().dataSources(
            (label) => (
              <a href="https://scryfall.com">{label}</a>
            ),
            (label) => (
              <a href="https://yawgatog.com/resources/magic-rules/">{label}</a>
            ),
          )}
        </p>
        <Show when={about()}>
          {(a) => (
            <p>
              {freshnessLine(a().freshness, m(), Math.max(0, Math.floor((now() - aboutAt) / 1000)))}
            </p>
          )}
        </Show>
        <SourceOffer m={m()} about={about() ?? null} />
      </footer>
    </main>
  );
}

/** The AGPL source offer: where this instance's code is, at which commit,
 * under which licence, and whom to write to about it. The facts come from the server (GET /api/about), so
 * an operator who points JUDGE_SOURCE_URL at their fork is covered here
 * without touching the page; while they are missing the footer says so
 * instead of guessing a repository. */
function SourceOffer(props: { m: Messages; about: About | null }) {
  return (
    <p class="source-offer">
      <Show
        when={props.about}
        fallback={props.m.offerUnanswered(
          (label) => <a href="https://www.gnu.org/licenses/agpl-3.0.html">{label}</a>,
          <code>GET /api/about</code>,
        )}
      >
        {(a) => (
          <>
            {props.m.offer({
              program: a().program,
              copyright: a().copyright,
              license: <a href={a().license_url}>{a().license_name}</a>,
              source: (label) => <a href={a().repository}>{label}</a>,
              commit: (
                <Show when={a().commit} fallback={props.m.commitUnknown}>
                  {(commit) => (
                    // A fragment, so the message is a tracked expression.
                    <>
                      {props.m.commitKnown(
                        <Show when={a().commit_url} fallback={<code>{commit().slice(0, 7)}</code>}>
                          {(url) => (
                            <a href={url()}>
                              <code>{commit().slice(0, 7)}</code>
                            </a>
                          )}
                        </Show>,
                        a().dirty,
                      )}
                    </>
                  )}
                </Show>
              ),
            })}
            <Show when={a().operator_email}>
              {(email) => (
                <>
                  {props.m.support(
                    <a href={`mailto:${email()}`}>{email()}</a>,
                    a().operator_discord ? <code>@{a().operator_discord}</code> : null,
                  )}
                </>
              )}
            </Show>
          </>
        )}
      </Show>
    </p>
  );
}

function Reply(props: {
  m: Messages;
  reply: ApiReply;
  onPick: (span: string, name: string) => void;
  onRetry: () => void;
  waiting: boolean;
}) {
  return (
    <Switch>
      <Match when={props.reply.kind === "answer" && props.reply}>
        {(r) => (
          <div class="answer">
            <p class="answer-text">
              <Markdown text={r().answer} />
            </p>
            <Show when={r().citations.length > 0}>
              <ul class="citations">
                <For each={r().citations}>
                  {(c) => (
                    <li>
                      <Show when={c.url} fallback={<span class="cite-label">{c.label}</span>}>
                        {(url) => (
                          <a
                            class="cite-label"
                            href={url()}
                            target="_blank"
                            rel="noopener noreferrer"
                          >
                            {c.label}
                          </a>
                        )}
                      </Show>{" "}
                      <span class="cite-quote">{props.m.quoted(<Symbols text={c.quote} />)}</span>
                    </li>
                  )}
                </For>
              </ul>
            </Show>
            <Show when={r().cards.length > 0}>
              <p class="meta cards">
                {props.m.cardsLabel}{" "}
                <For each={r().cards}>
                  {(card, j) => (
                    <>
                      {j() > 0 && props.m.separator}
                      <a href={card.url} target="_blank" rel="noopener noreferrer">
                        {card.name}
                      </a>
                    </>
                  )}
                </For>
              </p>
            </Show>
            <p class="meta">
              {props.m.answerMeta(
                props.m.confidence[r().confidence],
                props.m.crDate(r().cr_version),
                props.m.source[r().source],
              )}
            </p>
          </div>
        )}
      </Match>
      <Match when={props.reply.kind === "ambiguous" && props.reply}>
        {(r) => {
          const first = () => r().spans[0];
          return (
            <Show when={first()}>
              {(span) => (
                <div class="ambiguous">
                  <p>{props.m.whichCard(<strong>{span().query}</strong>)}</p>
                  <div class="choices">
                    <For each={span().choices}>
                      {(name) => (
                        <button type="button" onClick={() => props.onPick(span().query, name)}>
                          {name}
                        </button>
                      )}
                    </For>
                  </div>
                  <Show when={span().truncated}>
                    <p class="hint">{props.m.choicesTruncated}</p>
                  </Show>
                  <Show when={r().spans.length > 1}>
                    <p class="hint">
                      {props.m.askAfter(
                        r()
                          .spans.slice(1)
                          .map((s) => s.query),
                      )}
                    </p>
                  </Show>
                </div>
              )}
            </Show>
          );
        }}
      </Match>
      <Match when={props.reply.kind === "not_found" && props.reply}>
        {(r) => <p class="notice">{props.m.notFound(r().names)}</p>}
      </Match>
      <Match when={props.reply.kind === "maintenance" && props.reply}>
        {(r) => (
          <div class="maintenance">
            <p class="notice">{r().message}</p>
            <button type="button" onClick={() => props.onRetry()} disabled={props.waiting}>
              {props.m.tryAgain}
            </button>
          </div>
        )}
      </Match>
      <Match
        when={
          props.reply.kind !== "answer" &&
          props.reply.kind !== "ambiguous" &&
          props.reply.kind !== "not_found" &&
          props.reply.kind !== "maintenance" &&
          props.reply
        }
      >
        {(r) => {
          const failed = r() as Extract<ApiReply, { message: string }>;
          return <p class="notice">{failed.message}</p>;
        }}
      </Match>
    </Switch>
  );
}
