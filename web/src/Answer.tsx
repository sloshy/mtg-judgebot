import { createSignal, For, Match, onCleanup, onMount, Show, Switch } from "solid-js";
import type { CardView, CitationKind, CitationView } from "./api";
import { OutIcon } from "./Chat";
import Markdown from "./Markdown";
import type { Messages } from "./messages";
import Symbols from "./Symbols";

/** What an answer shows, from a question just asked or a stored call. */
export interface AnswerFacts {
  answer: string;
  confidence: "low" | "medium" | "high";
  source: "cr" | "commander";
  cr_version: string;
  citations: CitationView[];
  cards: CardView[];
}

/** A Comprehensive Rules citation is labelled with its rule number. */
const RULE_ID = /^\d{3}(\.\d+[a-z]?)?$/;

/** What a citation quotes: the server says, and an older server's is
 * guessed from the label (a rule number, or else something on a card). */
function kindOf(c: CitationView): CitationKind {
  // A kind this page does not know (a newer server) is guessed as well.
  if (c.kind !== undefined && KINDS.includes(c.kind)) return c.kind;
  return RULE_ID.test(c.label) ? "rule" : "oracle_text";
}

const KINDS: readonly CitationKind[] = ["rule", "ruling", "oracle_text", "prior_call"];

/** An answer as the judge's message: the text in a bubble, then its
 * citations as attachments, the cards it was about and the line under it. */
export default function Answer(props: {
  m: Messages;
  answer: AnswerFacts;
  /** Fold a long answer to its first lines, with a button to unfold it. */
  clamp?: boolean;
}) {
  const [open, setOpen] = createSignal(false);
  const [overflows, setOverflows] = createSignal(false);
  let text: HTMLDivElement | undefined;
  onMount(() => {
    if (props.clamp !== true || !text) return;
    const el = text;
    const check = () => setOverflows(el.scrollHeight > el.clientHeight + 2);
    const watch = new ResizeObserver(check);
    watch.observe(el);
    onCleanup(() => watch.disconnect());
  });
  const id = `answer-${Math.random().toString(36).slice(2)}`;
  const title = () => props.m.citationsTitle(props.answer.citations.length);
  const list = () => (
    <ul>
      <For each={props.answer.citations}>
        {(c) => (
          <li class="citation-row">
            <Citation m={props.m} citation={c} />
          </li>
        )}
      </For>
    </ul>
  );
  return (
    <>
      <div
        ref={text}
        id={id}
        class="bubble bubble-bot answer-text"
        classList={{ clamped: props.clamp === true && !open(), fades: overflows() && !open() }}
      >
        <Markdown text={props.answer.answer} />
      </div>
      <Show when={open() || overflows()}>
        <p class="unfold">
          <button
            type="button"
            class="link-button"
            aria-expanded={open()}
            aria-controls={id}
            onClick={() => setOpen(!open())}
          >
            {open() ? props.m.showLess : props.m.showMore}
          </button>
        </p>
      </Show>
      <Show when={props.answer.citations.length > 0}>
        {/* One section for all of them: closed, a line naming each; open,
            every citation in full. */}
        <details class="attachments attachments-folded">
          <summary>
            <span class="attachments-title">{title()}</span>
            <span class="attachments-labels" aria-hidden="true">
              <For each={props.answer.citations}>
                {(c) => <span class={`tag-${kindOf(c)}`}>{c.label}</span>}
              </For>
            </span>
            <Chevron />
          </summary>
          {list()}
        </details>
      </Show>
      <Show when={props.answer.cards.length > 0}>
        <p class="cards">
          <span class="cards-label">{props.m.cardsLabel}</span>
          <For each={props.answer.cards}>
            {(card) => (
              <a class="chip chip-card" href={card.url} target="_blank" rel="noopener noreferrer">
                {card.name}
                <OutIcon />
              </a>
            )}
          </For>
        </p>
      </Show>
      <p class="msg-meta">
        <span class={`confidence confidence-${props.answer.confidence}`} aria-hidden="true" />
        {props.m.answerMeta(
          props.m.confidence[props.answer.confidence],
          props.m.crDate(props.answer.cr_version),
          props.m.source[props.answer.source],
        )}
      </p>
    </>
  );
}

/** One citation: its kind, its label (a link to where it is, when it has a
 * public page) and the whole verbatim quote. */
function Citation(props: { m: Messages; citation: CitationView }) {
  const kind = () => kindOf(props.citation);
  return (
    <div class="citation">
      <span class={`kind kind-${kind()}`}>
        <Switch>
          <Match when={kind() === "rule"}>{props.m.ruleBadge}</Match>
          <Match when={kind() === "oracle_text"}>
            <CardGlyph />
          </Match>
          <Match when={kind() === "ruling"}>
            <RulingGlyph />
          </Match>
          <Match when={kind() === "prior_call"}>
            <PriorCallGlyph />
          </Match>
        </Switch>
      </span>
      <div class="citation-body">
        <p class="citation-label">
          <Show when={props.citation.url} fallback={props.citation.label}>
            {(url) => (
              <a href={url()} target="_blank" rel="noopener noreferrer">
                {props.citation.label}
                <OutIcon />
              </a>
            )}
          </Show>
        </p>
        <blockquote>{props.m.quoted(<Symbols text={props.citation.quote} />)}</blockquote>
      </div>
    </div>
  );
}

function Chevron() {
  return (
    <svg class="chevron" viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
      <path
        d="m4 6 4 4 4-4"
        fill="none"
        stroke="currentColor"
        stroke-width="1.8"
        stroke-linecap="round"
        stroke-linejoin="round"
      />
    </svg>
  );
}

function CardGlyph() {
  return (
    <svg viewBox="0 0 16 16" width="13" height="13" aria-hidden="true">
      <rect
        x="3.5"
        y="1.75"
        width="9"
        height="12.5"
        rx="1.5"
        fill="none"
        stroke="currentColor"
        stroke-width="1.5"
      />
      <path d="M6 5.5h4M6 8h4M6 10.5h2.5" stroke="currentColor" stroke-width="1.3" />
    </svg>
  );
}

/** A ruling: a card with a check mark on it. */
function RulingGlyph() {
  return (
    <svg viewBox="0 0 16 16" width="13" height="13" aria-hidden="true">
      <rect
        x="3.5"
        y="1.75"
        width="9"
        height="12.5"
        rx="1.5"
        fill="none"
        stroke="currentColor"
        stroke-width="1.5"
      />
      <path
        d="m5.8 8.2 1.6 1.6 3-3.2"
        fill="none"
        stroke="currentColor"
        stroke-width="1.5"
        stroke-linecap="round"
        stroke-linejoin="round"
      />
    </svg>
  );
}

/** An earlier answer: a speech bubble. */
function PriorCallGlyph() {
  return (
    <svg viewBox="0 0 16 16" width="13" height="13" aria-hidden="true">
      <path
        d="M3 3.5h10a1 1 0 0 1 1 1v6a1 1 0 0 1-1 1H8l-3 2.5v-2.5H3a1 1 0 0 1-1-1v-6a1 1 0 0 1 1-1z"
        fill="none"
        stroke="currentColor"
        stroke-width="1.5"
        stroke-linejoin="round"
      />
    </svg>
  );
}
