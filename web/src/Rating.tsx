// Rating an answer from the web (#27, docs/DECISIONS.md D38), and how one
// was rated. The three buttons sit under an answer in the chat, on the feed
// and on a call's page. Partially correct and Incorrect open a form under
// them (the kind of problem, what's wrong, the right answer, a name); the
// server says when the tab already rated the answer, and the page asks
// before replacing it. The tab's rater id is its session id (the one its
// questions share history under).
//
// Counts are Discord's: the web's are anonymous and never shape later
// answers, so they are shown only when the reader asks for them, a choice
// this browser remembers.

import { createSignal, For, Match, onMount, Show, Switch } from "solid-js";
import {
  type FeedEntry,
  type PublicReason,
  type RateReply,
  type RateRequest,
  type RatingCounts,
  rateCall,
  type Score,
} from "./api";
import type { Locale, Messages } from "./messages";
import { FEEDBACK_CATEGORIES } from "./router";

/** Where ratings go from this page: the tab's rater id, its language, and
 * whether reasons are shown publicly. Null until GET /api/about says
 * ratings are served here. */
const [rater, setRater] = createSignal<{
  session: string;
  locale: Locale | undefined;
  publicReasons: boolean;
} | null>(null);

/** Let the page rate, as `session`, replying in `locale`; `publicReasons`
 * when the operator shows reasons on the public pages. */
export function enableRating(
  session: string,
  locale: Locale | undefined,
  publicReasons: boolean,
): void {
  setRater({ session, locale, publicReasons });
}

const WEB_KEY = "judgebot-web-ratings";

function storedShowWeb(): boolean {
  try {
    return localStorage.getItem(WEB_KEY) === "shown";
  } catch {
    return false;
  }
}

const [showWeb, setShowWeb] = createSignal(storedShowWeb());

function toggleWeb(): void {
  const next = !showWeb();
  setShowWeb(next);
  try {
    localStorage.setItem(WEB_KEY, next ? "shown" : "hidden");
  } catch {
    // Remembered for this page only.
  }
}

const SCORES: readonly Score[] = ["correct", "partial", "incorrect"];

type Fault = "partial" | "incorrect";

type Step =
  | { step: "idle" }
  | { step: "form"; fault: Fault }
  | { step: "sending" }
  | { step: "confirm"; message: string; request: RateRequest; kept: string }
  | { step: "done"; message: string; tone: "ok" | "caution" };

/** The three buttons for call `call`, and what follows a click.
 * `onRated` hears a stored rating and the one it replaced. */
export function RateAnswer(props: {
  m: Messages;
  call: string;
  onRated?: (score: Score, replaced: Score | null) => void;
}) {
  const [step, setStep] = createSignal<Step>({ step: "idle" });
  const id = `rate-${props.call}`;
  let replaceButton: HTMLButtonElement | undefined;
  /** The buttons, to give focus back to the one that opened the form. */
  const buttons: Partial<Record<Score, HTMLButtonElement>> = {};
  const refocus = (score: Score) => queueMicrotask(() => buttons[score]?.focus());

  async function send(request: RateRequest) {
    const r = rater();
    if (r === null) return;
    setStep({ step: "sending" });
    const reply = await rateCall(props.call, request, r.session, r.locale, props.m);
    show(reply, request);
    // Back to the button the form came from, unless a question took focus.
    if (reply.kind !== "confirm") refocus(request.score);
  }

  function show(reply: RateReply, request: RateRequest) {
    switch (reply.kind) {
      case "rated":
        props.onRated?.(reply.score, reply.replaced);
        setStep({ step: "done", message: reply.message, tone: "ok" });
        return;
      case "already_rated":
        setStep({ step: "done", message: reply.message, tone: "ok" });
        return;
      case "confirm": {
        const earlier = reply.earlier;
        setStep({
          step: "confirm",
          message: reply.message,
          request,
          kept: props.m.rateKept(
            props.m.scoreName[earlier.score],
            earlier.category === null ? null : props.m.categoryName[earlier.category],
          ),
        });
        queueMicrotask(() => replaceButton?.focus());
        return;
      }
      default:
        setStep({ step: "done", message: reply.message, tone: "caution" });
    }
  }

  function click(score: Score) {
    if (score === "correct") void send({ score });
    else setStep({ step: "form", fault: score });
  }

  return (
    <Show when={rater()}>
      <div class="rate">
        <fieldset class="rate-buttons">
          <legend class="rate-ask">{props.m.rateAsk}</legend>
          <For each={SCORES}>
            {(score) => (
              <button
                type="button"
                ref={(el) => {
                  buttons[score] = el;
                }}
                class={`chip rate-${score}`}
                aria-pressed={
                  score === "correct"
                    ? undefined
                    : (() => {
                        const s = step();
                        return s.step === "form" && s.fault === score;
                      })()
                }
                aria-expanded={score === "correct" ? undefined : step().step === "form"}
                aria-controls={score === "correct" ? undefined : `${id}-form`}
                disabled={step().step === "sending"}
                onClick={() => click(score)}
              >
                {props.m.scoreName[score]}
              </button>
            )}
          </For>
        </fieldset>
        <Switch>
          <Match
            when={(() => {
              const s = step();
              return s.step === "form" ? s : false;
            })()}
          >
            {(s) => (
              <ReasonForm
                m={props.m}
                id={`${id}-form`}
                fault={s().fault}
                publicReasons={rater()?.publicReasons === true}
                onCancel={() => {
                  const fault = s().fault;
                  setStep({ step: "idle" });
                  refocus(fault);
                }}
                onSend={(request) => void send(request)}
              />
            )}
          </Match>
          <Match when={step().step === "sending"}>
            <p class="rate-status" role="status">
              {props.m.rateSending}
            </p>
          </Match>
          <Match
            when={(() => {
              const s = step();
              return s.step === "confirm" ? s : false;
            })()}
          >
            {(s) => (
              <div class="rate-confirm">
                <p role="status">{s().message}</p>
                <p class="rate-actions">
                  <button
                    ref={replaceButton}
                    type="button"
                    class="chip chip-action"
                    onClick={() => void send({ ...s().request, replace: true })}
                  >
                    {props.m.rateReplace}
                  </button>
                  <button
                    type="button"
                    class="chip"
                    onClick={() => setStep({ step: "done", message: s().kept, tone: "ok" })}
                  >
                    {props.m.rateKeep}
                  </button>
                </p>
              </div>
            )}
          </Match>
          <Match
            when={(() => {
              const s = step();
              return s.step === "done" ? s : false;
            })()}
          >
            {(s) => (
              <p class={`rate-status rate-status-${s().tone}`} role="status">
                {s().message}
              </p>
            )}
          </Match>
        </Switch>
      </div>
    </Show>
  );
}

/** The reason for a fault: its kind (required), what's wrong, the right
 * answer and a name (all optional). Focus moves to its first field. */
function ReasonForm(props: {
  m: Messages;
  id: string;
  fault: Fault;
  publicReasons: boolean;
  onCancel: () => void;
  onSend: (request: RateRequest) => void;
}) {
  let category: HTMLSelectElement | undefined;
  let wrong: HTMLTextAreaElement | undefined;
  let right: HTMLTextAreaElement | undefined;
  let name: HTMLInputElement | undefined;
  onMount(() => category?.focus());
  const value = (el: HTMLInputElement | HTMLTextAreaElement | undefined) => {
    const v = el?.value.trim() ?? "";
    return v === "" ? undefined : v;
  };
  return (
    <form
      id={props.id}
      class="rate-form"
      aria-label={props.m.rateFormTitle(props.fault)}
      onSubmit={(e) => {
        e.preventDefault();
        const chosen = FEEDBACK_CATEGORIES.find((c) => c === category?.value);
        if (chosen === undefined) {
          category?.reportValidity();
          return;
        }
        props.onSend({
          score: props.fault,
          category: chosen,
          wrong: value(wrong),
          right: value(right),
          name: value(name),
        });
      }}
      onKeyDown={(e) => {
        if (e.key === "Escape") props.onCancel();
      }}
    >
      <p class="rate-form-title">{props.m.rateFormTitle(props.fault)}</p>
      <label class="rate-field">
        <span>{props.m.rateCategory}</span>
        <select ref={category} name="category" required>
          <option value="">{props.m.rateChoose}</option>
          <For each={FEEDBACK_CATEGORIES}>
            {(c) => <option value={c}>{props.m.categoryName[c]}</option>}
          </For>
        </select>
      </label>
      <label class="rate-field">
        <span>
          {props.m.rateWrong} <span class="rate-optional">{props.m.rateOptional}</span>
        </span>
        <textarea ref={wrong} name="wrong" rows={3} maxLength={1000} />
      </label>
      <label class="rate-field">
        <span>
          {props.m.rateRight} <span class="rate-optional">{props.m.rateOptional}</span>
        </span>
        <textarea ref={right} name="right" rows={3} maxLength={1000} />
      </label>
      <label class="rate-field">
        <span>
          {props.m.rateName} <span class="rate-optional">{props.m.rateOptional}</span>
        </span>
        <input ref={name} name="name" maxLength={100} autocomplete="nickname" />
      </label>
      <p class="rate-note">
        {props.publicReasons ? props.m.rateFormNotePublic : props.m.rateFormNote}
      </p>
      <p class="rate-actions">
        <button type="submit" class="chip chip-action">
          {props.m.rateSend}
        </button>
        <button type="button" class="chip" onClick={() => props.onCancel()}>
          {props.m.rateCancel}
        </button>
      </p>
    </form>
  );
}

function add(a: RatingCounts, b: RatingCounts): RatingCounts {
  return {
    correct: a.correct + b.correct,
    partial: a.partial + b.partial,
    incorrect: a.incorrect + b.incorrect,
  };
}

function total(c: RatingCounts): number {
  return c.correct + c.partial + c.incorrect;
}

/** `ratings` with a web rating of `score` added and `replaced` taken away. */
export function withWebRating(
  ratings: FeedEntry["ratings"],
  score: Score,
  replaced: Score | null,
): FeedEntry["ratings"] {
  const web = { ...ratings.web };
  web[score] += 1;
  if (replaced !== null) web[replaced] = Math.max(0, web[replaced] - 1);
  return { ...ratings, web };
}

/** How a call was rated: Discord's counts, the web's too when the reader
 * includes them, and the button that includes or hides them. */
export function RatingsLine(props: { m: Messages; ratings: FeedEntry["ratings"] }) {
  const web = () => total(props.ratings.web);
  const shown = () =>
    showWeb() ? add(props.ratings.discord, props.ratings.web) : props.ratings.discord;
  return (
    <>
      {props.m.ratingsLine(shown(), props.ratings.judge)}
      <Show when={web() > 0}>
        {" "}
        <button type="button" class="link-button" aria-pressed={showWeb()} onClick={toggleWeb}>
          {showWeb() ? props.m.hideWebRatings : props.m.includeWebRatings(web())}
        </button>
      </Show>
    </>
  );
}

/** Raters' reasons, when the operator shows them: the kind of problem and
 * the rater's words, never who they are. The web's follow the counts'
 * choice. */
export function Reasons(props: { m: Messages; reasons: PublicReason[] }) {
  const shown = () => props.reasons.filter((r) => r.rater_kind === "discord" || showWeb());
  return (
    <Show when={shown().length > 0}>
      <section class="reasons" aria-label={props.m.reasonsTitle}>
        <p class="reasons-title">{props.m.reasonsTitle}</p>
        <ul>
          <For each={shown()}>
            {(r) => (
              <li class="reason">
                <p class="reason-head">
                  <span class="badge">{props.m.categoryName[r.category]}</span>{" "}
                  {props.m.scoreName[r.score]}
                </p>
                <Show when={r.wrong}>
                  {(text) => (
                    <p>
                      <strong>{props.m.rateWrong}:</strong> {text()}
                    </p>
                  )}
                </Show>
                <Show when={r.right}>
                  {(text) => (
                    <p>
                      <strong>{props.m.rateRight}:</strong> {text()}
                    </p>
                  )}
                </Show>
              </li>
            )}
          </For>
        </ul>
      </section>
    </Show>
  );
}
