import { For, Show } from "solid-js";
import type { CardView, CitationView } from "./api";
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

/** An answer with its citations, the cards it was about and the line under it. */
export default function Answer(props: { m: Messages; answer: AnswerFacts }) {
  return (
    <div class="answer">
      <p class="answer-text">
        <Markdown text={props.answer.answer} />
      </p>
      <Show when={props.answer.citations.length > 0}>
        <ul class="citations">
          <For each={props.answer.citations}>
            {(c) => (
              <li>
                <Show when={c.url} fallback={<span class="cite-label">{c.label}</span>}>
                  {(url) => (
                    <a class="cite-label" href={url()} target="_blank" rel="noopener noreferrer">
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
      <Show when={props.answer.cards.length > 0}>
        <p class="meta cards">
          {props.m.cardsLabel}{" "}
          <For each={props.answer.cards}>
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
          props.m.confidence[props.answer.confidence],
          props.m.crDate(props.answer.cr_version),
          props.m.source[props.answer.source],
        )}
      </p>
    </div>
  );
}
