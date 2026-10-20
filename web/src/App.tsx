import "@fontsource-variable/figtree";
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
import Answer from "./Answer";
import {
  type About,
  type ApiReply,
  askJudge,
  fetchAbout,
  freshnessLine,
  type Pin,
  sessionId,
} from "./api";
import { BotAvatar, BotMessage, Notice, Typing } from "./Chat";
import { ThemeSelect } from "./Controls";
import { DashboardPage } from "./Dashboard";
import { CallPage, FeedPage } from "./Feed";
import { DOCS_URL } from "./links";
import { isLocale, type Locale, type Messages, messages, pickLocale } from "./messages";
import { enableRating, RateAnswer } from "./Rating";
import { follow, href, route } from "./router";
import Symbols from "./Symbols";

const MAX_QUESTION_CHARS = 1000;
/** The question box grows with its text up to this many pixels, then scrolls. */
const MAX_BOX_HEIGHT = 200;

interface Entry {
  question: string;
  pins: Pin[];
  /** null while the judge is thinking. */
  reply: ApiReply | null;
}

/** Scroll to the exchange `i` (its question at the top, under the bar). */
function reveal(i: number) {
  requestAnimationFrame(() => {
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    document
      .getElementById(`exchange-${i}`)
      ?.scrollIntoView({ block: "start", behavior: reduce ? "auto" : "smooth" });
  });
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
  // Rating buttons appear once the server says ratings are served here; the
  // tab rates under the session id its questions share history under.
  createEffect(() => {
    const a = about();
    if (a?.ratings === true) enableRating(session, browserLocale, a.public_feedback === true);
  });

  async function ask(question: string, pins: Pin[], index?: number) {
    const i = index ?? entries.length;
    if (index === undefined) {
      setEntries(i, { question, pins, reply: null });
      reveal(i);
    } else {
      setEntries(i, { pins, reply: null });
    }
    setWaiting(true);
    try {
      const reply = await askJudge(question, pins, session, browserLocale, m());
      setEntries(i, "reply", reply);
      // Start the reply where its question is, not wherever it ends.
      reveal(i);
    } finally {
      setWaiting(false);
      // The box was disabled while the judge worked, which took the focus
      // from it; give it back for the next question, except on a touch
      // screen, where it would raise the keyboard over the answer.
      if (document.activeElement === document.body && !matchMedia("(pointer: coarse)").matches) {
        box?.focus({ preventScroll: true });
      }
    }
  }

  function submit(e: SubmitEvent) {
    e.preventDefault();
    const question = draft().trim();
    if (!question || waiting()) return;
    setDraft("");
    void ask(question, []);
  }

  /** Grow the box with its text (where CSS `field-sizing` cannot). */
  function fit() {
    if (!box || CSS.supports("field-sizing", "content")) return;
    box.style.height = "auto";
    box.style.height = `${Math.min(box.scrollHeight, MAX_BOX_HEIGHT)}px`;
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

  createEffect(() => {
    draft();
    fit();
  });

  // Links between the page's own routes move without a reload (`follow`).
  document.addEventListener("click", follow);
  onCleanup(() => document.removeEventListener("click", follow));

  // The feed's pages exist only where the server serves the feed.
  const feed = () => about()?.feed === true;
  // The dashboard's, likewise (it follows the feed on the server).
  const stats = () => about()?.stats ?? null;

  return (
    <div class="app">
      <a class="skip-link" href="#main">
        {m().skipToContent}
      </a>
      <header class="topbar">
        <div class="topbar-inner">
          <h1 class="brand">
            <a href={href({ page: "ask" })}>
              <BotAvatar />
              <span class="brand-name">{m().title}</span>
            </a>
          </h1>
          <div class="topbar-end">
            <Show when={feed()}>
              <nav class="tabs" aria-label={m().navLabel}>
                <a
                  href={href({ page: "ask" })}
                  aria-current={route().page === "ask" ? "page" : undefined}
                >
                  {m().navAsk}
                </a>
                <a
                  href={href({ page: "feed" })}
                  aria-current={
                    route().page === "feed" || route().page === "call" || route().page === "user"
                      ? "page"
                      : undefined
                  }
                >
                  {m().navFeed}
                </a>
                <Show when={stats()}>
                  <a
                    href={href({ page: "dashboard" })}
                    aria-current={route().page === "dashboard" ? "page" : undefined}
                  >
                    {m().navDashboard}
                  </a>
                </Show>
              </nav>
            </Show>
            <a class="docs-link" href={DOCS_URL}>
              <svg viewBox="0 0 20 20" width="18" height="18" aria-hidden="true">
                <path
                  d="M3 4.5c2.5-1 5-1 7 .8 2-1.8 4.5-1.8 7-.8v11c-2.5-1-5-1-7 .8-2-1.8-4.5-1.8-7-.8zM10 5.3v11"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.6"
                  stroke-linejoin="round"
                />
              </svg>
              <span class="docs-label">{m().navDocs}</span>
            </a>
            <ThemeSelect m={m()} />
          </div>
        </div>
      </header>

      <main class="main" id="main" tabIndex={-1}>
        <Switch>
          <Match when={route().page === "ask"}>{null}</Match>
          <Match
            when={
              !about.loading &&
              ((!feed() && route().page !== "missing" && route().page !== "admin") ||
                (route().page === "dashboard" && stats() === null))
            }
          >
            <Missing m={m()} />
          </Match>
          <Match when={about.loading}>
            <section class="page">
              <Typing m={m()} loading={true} />
            </section>
          </Match>
          <Match
            when={(() => {
              const r = route();
              return r.page === "feed" ? r : false;
            })()}
          >
            {(r) => (
              <FeedPage
                m={m()}
                locale={browserLocale}
                query={{ interface: r().interface, user: r().user, q: r().q }}
                title={m().feedTitle}
                filters={true}
              />
            )}
          </Match>
          <Match
            when={(() => {
              const r = route();
              return r.page === "user" ? r : false;
            })()}
          >
            {(r) => (
              <FeedPage
                m={m()}
                locale={browserLocale}
                query={{ user: r().name }}
                title={m().userTitle(r().name)}
                filters={false}
                user={r().name}
                dashboard={stats() !== null}
              />
            )}
          </Match>
          <Match
            when={(() => {
              const r = route();
              return r.page === "dashboard" ? r : false;
            })()}
          >
            {(r) => (
              <DashboardPage
                m={m()}
                locale={browserLocale}
                route={r()}
                models={stats()?.models === true}
              />
            )}
          </Match>
          <Match
            when={(() => {
              const r = route();
              return r.page === "call" ? r : false;
            })()}
          >
            {(r) => <CallPage m={m()} locale={browserLocale} id={r().id} />}
          </Match>
          {/* The admin pages are not this app's: whatever reaches here as
              one (served by the public listener) is a missing page. */}
          <Match when={route().page === "missing" || route().page === "admin"}>
            <Missing m={m()} />
          </Match>
        </Switch>

        {/* Kept mounted off its route, so a conversation survives a look at the feed. */}
        <div class="ask-view" hidden={route().page !== "ask"}>
          <section class="conversation" aria-label={m().conversationLabel}>
            {/* The judge introduces itself; once the visitor has asked, the
                introduction stays as its first message. */}
            <Show
              when={entries.length === 0}
              fallback={
                <BotMessage m={m()}>
                  <p class="bubble bubble-bot">{m().tagline}</p>
                </BotMessage>
              }
            >
              <div class="greeting">
                <BotAvatar size={64} />
                <h2>{m().title}</h2>
                <p>{m().tagline}</p>
              </div>
            </Show>

            <Show when={entries.length === 0}>
              <section class="suggestions" aria-labelledby="examples-title">
                <h3 id="examples-title">{m().examplesTitle}</h3>
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

            <For each={entries}>
              {(entry, i) => (
                <div class="exchange" id={`exchange-${i()}`}>
                  <div class="msg-user">
                    <p class="bubble bubble-user">
                      <span class="sr-only">{m().you}</span>
                      <Symbols text={entry.question} />
                    </p>
                    <For each={entry.pins}>
                      {(pin) => <p class="pin">{m().pinned(pin.span, pin.name)}</p>}
                    </For>
                  </div>
                  <div aria-live="polite">
                    <Show when={entry.reply} fallback={<Typing m={m()} />}>
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
                  </div>
                </div>
              )}
            </For>
          </section>

          <form class="composer" onSubmit={submit}>
            <div class="composer-box">
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
                rows="1"
                aria-label={m().placeholder}
                placeholder={m().placeholder}
                disabled={waiting()}
              />
              <button type="submit" class="send" disabled={waiting() || !draft().trim()}>
                <span class="send-label">{waiting() ? m().judging : m().ask}</span>
                <Show
                  when={waiting()}
                  fallback={
                    <svg viewBox="0 0 20 20" width="20" height="20" aria-hidden="true">
                      <path
                        d="M10 16V4M4.5 9.5 10 4l5.5 5.5"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="2.2"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                      />
                    </svg>
                  }
                >
                  <span class="spinner" aria-hidden="true" />
                </Show>
              </button>
            </div>
            <p class="composer-hint">
              <Show when={feed()}>
                <span>{m().askIsPublic}</span>
              </Show>
              <span class="keys">{m().keyHint}</span>
            </p>
          </form>
        </div>
      </main>

      <footer class="site-footer">
        <div class="footer-inner">
          <div class="footer-lead">
            <p class="footer-brand">
              <BotAvatar />
              <span>{m().title}</span>
            </p>
            <p class="footer-warning">{m().notRated}</p>
            <Show when={about()}>
              {(a) => (
                <p class="footer-fresh">
                  {freshnessLine(
                    a().freshness,
                    m(),
                    Math.max(0, Math.floor((now() - aboutAt) / 1000)),
                  )}
                </p>
              )}
            </Show>
          </div>
          <ul class="footer-list">
            <li>
              {m().fanContent((label) => (
                <a href="https://company.wizards.com/en/legal/fancontentpolicy">{label}</a>
              ))}
            </li>
            <li>
              {m().dataSources(
                (label) => (
                  <a href="https://scryfall.com">{label}</a>
                ),
                (label) => (
                  <a href="https://yawgatog.com/resources/magic-rules/">{label}</a>
                ),
              )}
            </li>
            <li>
              <SourceOffer m={m()} about={about() ?? null} />
            </li>
          </ul>
        </div>
      </footer>
    </div>
  );
}

/** A route that names nothing, said by the judge with a way back. */
function Missing(props: { m: Messages }) {
  return (
    <section class="page">
      <BotMessage m={props.m}>
        <Notice tone="note" title={props.m.missingTitle}>
          <p>{props.m.pageMissing}</p>
          <p class="quick-replies">
            <a class="chip chip-action" href={href({ page: "ask" })}>
              {props.m.backToAsk}
            </a>
          </p>
        </Notice>
      </BotMessage>
    </section>
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
    <BotMessage m={props.m}>
      <Switch>
        <Match when={props.reply.kind === "answer" && props.reply}>
          {(r) => (
            <>
              <Answer m={props.m} answer={r()} />
              <Show when={r().call}>{(call) => <RateAnswer m={props.m} call={call()} />}</Show>
            </>
          )}
        </Match>
        <Match when={props.reply.kind === "ambiguous" && props.reply}>
          {(r) => {
            const first = () => r().spans[0];
            return (
              <Show when={first()}>
                {(span) => (
                  <>
                    <div class="bubble bubble-bot">
                      <p>{props.m.whichCard(<strong>{span().query}</strong>)}</p>
                      <Show when={span().truncated}>
                        <p class="bubble-note">{props.m.choicesTruncated}</p>
                      </Show>
                      <Show when={r().spans.length > 1}>
                        <p class="bubble-note">
                          {props.m.askAfter(
                            r()
                              .spans.slice(1)
                              .map((s) => s.query),
                          )}
                        </p>
                      </Show>
                    </div>
                    <div class="quick-replies">
                      <For each={span().choices}>
                        {(name) => (
                          <button
                            type="button"
                            class="chip chip-action"
                            disabled={props.waiting}
                            onClick={() => props.onPick(span().query, name)}
                          >
                            {name}
                          </button>
                        )}
                      </For>
                    </div>
                  </>
                )}
              </Show>
            );
          }}
        </Match>
        <Match when={props.reply.kind === "not_found" && props.reply}>
          {(r) => (
            <Notice tone="caution" title={props.m.replyTitle.not_found}>
              <p>{props.m.notFound(r().names)}</p>
            </Notice>
          )}
        </Match>
        <Match when={props.reply.kind === "maintenance" && props.reply}>
          {(r) => (
            <Notice tone="note" title={props.m.replyTitle.maintenance}>
              <p>{r().message}</p>
              <p class="quick-replies">
                <button
                  type="button"
                  class="chip chip-action"
                  onClick={() => props.onRetry()}
                  disabled={props.waiting}
                >
                  {props.m.tryAgain}
                </button>
              </p>
            </Notice>
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
            const failure = () =>
              r() as Extract<ApiReply, { kind: "error" | "busy" | "rate_limited" }>;
            return (
              <Notice
                tone={failure().kind === "error" ? "danger" : "caution"}
                title={props.m.replyTitle[failure().kind]}
              >
                <p>{failure().message}</p>
              </Notice>
            );
          }}
        </Match>
      </Switch>
    </BotMessage>
  );
}
