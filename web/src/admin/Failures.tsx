// /admin/failures: the calls that failed for a reason to look into, each a
// line that opens onto the error and what the model sent.

import { createResource, For, Match, Show, Switch } from "solid-js";
import { failed, succeeded } from "../api";
import { PersonAvatar, Typing } from "../Chat";
import { type AdminContext, ReadFailure } from "./Admin";
import { type Failure, fetchFailures } from "./api";
import { askerText } from "./Calls";

export function FailuresPage(props: { ctx: AdminContext }) {
  const m = () => props.ctx.m;
  const [read] = createResource(() => fetchFailures(props.ctx.token, m()));
  return (
    <section class="page">
      <header class="page-head">
        <div>
          <h2>{m().failuresTitle}</h2>
          <p class="page-intro">{m().failuresIntro}</p>
        </div>
      </header>
      <Switch fallback={<Typing m={props.ctx.pub} loading={true} />}>
        <Match when={succeeded(read())}>
          {(got) => {
            const failures = () => got().failures;
            return (
              <Show
                when={failures().length > 0}
                fallback={<p class="admin-empty">{m().failuresEmpty}</p>}
              >
                <ul class="rows">
                  <For each={failures()}>
                    {(f) => (
                      <li>
                        <FailureRow ctx={props.ctx} failure={f} />
                      </li>
                    )}
                  </For>
                </ul>
              </Show>
            );
          }}
        </Match>
        <Match when={failed(read())}>{(f) => <ReadFailure ctx={props.ctx} read={f()} />}</Match>
      </Switch>
    </section>
  );
}

function FailureRow(props: { ctx: AdminContext; failure: Failure }) {
  const m = () => props.ctx.m;
  const f = () => props.failure;
  return (
    <details class="row">
      <summary class="row-line">
        <PersonAvatar name={f().asker} />
        <span class="row-question">{f().question}</span>
        <span class="row-facts">
          <span class="badge">{m().interfaceName[f().interface]}</span>
          <Show when={f().private}>
            <span class="tag-private">{m().privateTag}</span>
          </Show>
          <span>{askerText(m(), f().asker, null)}</span>
          <time datetime={f().at}>{m().when(f().at)}</time>
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
      <div class="row-body admin-failure">
        <h4>{m().failureError}</h4>
        <pre>{f().error}</pre>
        <Show when={f().first_rejection}>
          {(why) => (
            <>
              <h4>{m().failureRejection}</h4>
              <pre>{why()}</pre>
            </>
          )}
        </Show>
        <For each={f().attempts}>
          {(attempt, i) => (
            <>
              <h4>{m().failureAttempt(i() + 1)}</h4>
              <pre>{attempt}</pre>
            </>
          )}
        </For>
      </div>
    </details>
  );
}
