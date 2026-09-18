import { createResource, createSignal, For, Match, Show, Switch } from "solid-js";
import { createStore } from "solid-js/store";
import {
  type About,
  type ApiReply,
  askJudge,
  crDate,
  fetchAbout,
  type Pin,
  sessionId,
} from "./api";
import Symbols from "./Symbols";

const MAX_QUESTION_CHARS = 1000;

interface Entry {
  question: string;
  pins: Pin[];
  /** null while the judge is thinking. */
  reply: ApiReply | null;
}

const CONFIDENCE_LABEL = { low: "Low", medium: "Medium", high: "High" } as const;
const SOURCE_LABEL = { cr: "Comprehensive Rules", commander: "Commander rules" } as const;

export default function App() {
  const [entries, setEntries] = createStore<Entry[]>([]);
  const [draft, setDraft] = createSignal("");
  const [waiting, setWaiting] = createSignal(false);
  const session = sessionId();
  // `null` while GET /api/about is pending or after it failed; the footer
  // then says so rather than naming a repository this instance may not be.
  const [about] = createResource(fetchAbout);

  async function ask(question: string, pins: Pin[], index?: number) {
    const i = index ?? entries.length;
    if (index === undefined) {
      setEntries(i, { question, pins, reply: null });
    } else {
      setEntries(i, { pins, reply: null });
    }
    setWaiting(true);
    try {
      const reply = await askJudge(question, pins, session);
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

  /** A "did you mean…?" pick: re-ask the same entry with the span pinned. */
  function pick(index: number, span: string, name: string) {
    const entry = entries[index];
    if (!entry || waiting()) return;
    void ask(entry.question, [...entry.pins, { span, name }], index);
  }

  return (
    <main class="app">
      <header>
        <h1>MTG Judgebot</h1>
        <p class="tagline">
          Ask a Magic: The Gathering rules question; the answer cites the Comprehensive Rules.
        </p>
      </header>

      <For each={entries}>
        {(entry, i) => (
          <section class="exchange">
            <p class="question">
              <Symbols text={entry.question} />
            </p>
            <Show
              when={entry.reply}
              fallback={
                <p class="thinking">Consulting the rules… this usually takes 20–45 seconds.</p>
              }
            >
              {(reply) => <Reply reply={reply()} onPick={(span, name) => pick(i(), span, name)} />}
            </Show>
          </section>
        )}
      </For>

      <form class="ask" onSubmit={submit}>
        <textarea
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
          placeholder="Does lifelink stack? Use brackets like [[Full Card Name]] to avoid ambiguity."
          disabled={waiting()}
        />
        <button type="submit" disabled={waiting() || !draft().trim()}>
          {waiting() ? "Judging…" : "Ask the judge"}
        </button>
      </form>

      <footer>
        <p>
          Answers here can't be rated. Answers are AI-generated; verify anything important with a
          human judge.
        </p>
        <p>
          MTG Judgebot is unofficial Fan Content permitted under the{" "}
          <a href="https://company.wizards.com/en/legal/fancontentpolicy">Fan Content Policy</a>.
          Not approved or endorsed by Wizards of the Coast. Portions of the materials used are
          property of Wizards of the Coast. ©Wizards of the Coast LLC.
        </p>
        <p>
          The Comprehensive Rules come from Wizards of the Coast. Card data, rulings and card
          symbols come from <a href="https://scryfall.com">Scryfall</a>, which is not affiliated
          with this project. Rule links go to the{" "}
          <a href="https://yawgatog.com/resources/magic-rules/">Yawgatog</a> mirror.
        </p>
        <SourceOffer about={about() ?? null} />
      </footer>
    </main>
  );
}

/** The AGPL source offer: where this instance's code is, at which commit,
 * under which licence, and whom to write to about it. The facts come from the server (GET /api/about), so
 * an operator who points JUDGE_SOURCE_URL at their fork is covered here
 * without touching the page; while they are missing the footer says so
 * instead of guessing a repository. */
function SourceOffer(props: { about: About | null }) {
  return (
    <p class="source-offer">
      <Show
        when={props.about}
        fallback={
          <>
            Free software under the{" "}
            <a href="https://www.gnu.org/licenses/agpl-3.0.html">
              GNU Affero General Public License
            </a>
            . The source offer for this instance (repository and commit) comes from{" "}
            <code>GET /api/about</code>, which has not answered.
          </>
        }
      >
        {(a) => (
          <>
            {a().program} — {a().copyright}. Free software under the{" "}
            <a href={a().license_url}>{a().license_name}</a>; anyone offering a modified version
            over a network must offer its source under the same licence.{" "}
            <a href={a().repository}>Source code of this instance</a>{" "}
            <Show when={a().commit} fallback={<>(commit unknown)</>}>
              {(commit) => (
                <>
                  (commit{" "}
                  <Show when={a().commit_url} fallback={<code>{commit().slice(0, 7)}</code>}>
                    {(url) => (
                      <a href={url()}>
                        <code>{commit().slice(0, 7)}</code>
                      </a>
                    )}
                  </Show>
                  <Show when={a().dirty}>, built with uncommitted changes</Show>)
                </>
              )}
            </Show>
            .
            <Show when={a().operator_email}>
              {(email) => (
                <>
                  {" "}
                  For support, contact whoever runs this instance:{" "}
                  <a href={`mailto:${email()}`}>{email()}</a>
                  <Show when={a().operator_discord}>
                    {(discord) => (
                      <>
                        , or <code>@{discord()}</code> on Discord
                      </>
                    )}
                  </Show>
                  .
                </>
              )}
            </Show>
          </>
        )}
      </Show>
    </p>
  );
}

function Reply(props: { reply: ApiReply; onPick: (span: string, name: string) => void }) {
  return (
    <Switch>
      <Match when={props.reply.kind === "answer" && props.reply}>
        {(r) => (
          <div class="answer">
            <p class="answer-text">
              <Symbols text={r().answer} />
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
                      <span class="cite-quote">
                        “<Symbols text={c.quote} />”
                      </span>
                    </li>
                  )}
                </For>
              </ul>
            </Show>
            <Show when={r().cards.length > 0}>
              <p class="meta cards">
                Cards:{" "}
                <For each={r().cards}>
                  {(card, j) => (
                    <>
                      {j() > 0 && " · "}
                      <a href={card.url} target="_blank" rel="noopener noreferrer">
                        {card.name}
                      </a>
                    </>
                  )}
                </For>
              </p>
            </Show>
            <p class="meta">
              Confidence: {CONFIDENCE_LABEL[r().confidence]} · CR {crDate(r().cr_version)} ·{" "}
              {SOURCE_LABEL[r().source]}
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
                  <p>
                    I'm not sure which card you mean by <strong>{span().query}</strong>. Did you
                    mean…?
                  </p>
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
                    <p class="hint">
                      Showing the first five. If yours isn't here, re-ask writing its full name as
                      [[Full Card Name]].
                    </p>
                  </Show>
                  <Show when={r().spans.length > 1}>
                    <p class="hint">
                      After that I'll ask about{" "}
                      {r()
                        .spans.slice(1)
                        .map((s) => s.query)
                        .join(", ")}
                      .
                    </p>
                  </Show>
                </div>
              )}
            </Show>
          );
        }}
      </Match>
      <Match when={props.reply.kind === "not_found" && props.reply}>
        {(r) => (
          <p class="notice">
            I couldn't find a card called {r().names.join(" or ")}. Check the spelling, or write the
            full name as [[Full Card Name]].
          </p>
        )}
      </Match>
      <Match
        when={
          props.reply.kind !== "answer" &&
          props.reply.kind !== "ambiguous" &&
          props.reply.kind !== "not_found" &&
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
