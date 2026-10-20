// /admin/calls/<id>: one stored call whole. The exchange as the feed shows
// it, then what is stored with it: its origin, every rating with its rater,
// and the run that answered it with each billed request.

import { createResource, For, Match, Show, Switch } from "solid-js";
import Answer from "../Answer";
import { BotMessage, Message, Notice, PersonAvatar, Typing } from "../Chat";
import { href } from "../router";
import Symbols from "../Symbols";
import { type AdminContext, ReadFailure } from "./Admin";
import { type CallDetail, fetchCall, type Run } from "./api";
import { askerText } from "./Calls";

export function CallPage(props: { ctx: AdminContext; id: string }) {
  const m = () => props.ctx.m;
  const [call] = createResource(
    () => props.id,
    (id) => fetchCall(id, props.ctx.token, m()),
  );
  return (
    <section class="page">
      <p class="back">
        <a href={href({ page: "admin", view: { view: "calls", filter: {} } })}>
          <span aria-hidden="true">←</span>
          {m().backToCalls}
        </a>
      </p>
      <Switch fallback={<Typing m={props.ctx.pub} loading={true} />}>
        <Match when={call()?.ok === true && call()}>
          {(r) => {
            const read = r();
            return read.ok ? <Detail ctx={props.ctx} call={read.value} /> : null;
          }}
        </Match>
        <Match when={call()?.ok === false && call()}>
          {(r) => {
            const read = r();
            if (read.ok) return null;
            return read.status === 404 ? (
              <BotMessage m={props.ctx.pub}>
                <Notice tone="note" title={m().callMissingTitle}>
                  <p>{read.message}</p>
                </Notice>
              </BotMessage>
            ) : (
              <ReadFailure ctx={props.ctx} read={read} />
            );
          }}
        </Match>
      </Switch>
    </section>
  );
}

function Detail(props: { ctx: AdminContext; call: CallDetail }) {
  const m = () => props.ctx.m;
  const c = () => props.call;
  return (
    <>
      <article class="exchange">
        <Message
          class="msg-asker"
          avatar={<PersonAvatar name={c().asker} />}
          name={askerText(m(), c().asker, c().asker_id)}
          header={
            <>
              <span class="badge">{m().interfaceName[c().interface]}</span>
              <Show when={c().private}>
                <span class="tag-private">{m().privateTag}</span>
              </Show>
              <time class="msg-time" datetime={c().asked_at}>
                {m().when(c().asked_at)}
              </time>
            </>
          }
        >
          <p class="bubble bubble-asker">
            <Symbols text={c().question} />
          </p>
        </Message>
        <BotMessage m={props.ctx.pub}>
          <Answer m={props.ctx.pub} answer={c()} />
        </BotMessage>
      </article>

      <section class="admin-section" aria-labelledby="facts-title">
        <h3 id="facts-title">{m().factsTitle}</h3>
        <dl class="admin-facts">
          <dt>{m().factAsked}</dt>
          <dd>
            <time datetime={c().asked_at}>{m().when(c().asked_at)}</time>
          </dd>
          <dt>{m().factInterface}</dt>
          <dd>{m().interfaceName[c().interface]}</dd>
          <dt>{m().factAsker}</dt>
          <dd>
            {askerText(m(), c().asker, c().asker_id)}
            <Show when={c().asker !== null && c().asker_id}>
              {(id) => <code class="admin-id">{id()}</code>}
            </Show>
          </dd>
          <dt>{m().factVisibility}</dt>
          <dd>{c().private ? m().privateTag : m().publicTag}</dd>
          <dt>{m().factThread}</dt>
          <dd>
            <code>{c().thread}</code>
          </dd>
          <dt>{m().factLocale}</dt>
          <dd>{c().locale}</dd>
          <Show when={c().session}>
            {(session) => (
              <>
                <dt>{m().factSession}</dt>
                <dd>
                  <code>{session()}</code>
                </dd>
              </>
            )}
          </Show>
          <dt>{m().factCategory}</dt>
          <dd>{c().category}</dd>
          <dt>{m().factRules}</dt>
          <dd>{props.ctx.pub.crDate(c().cr_version)}</dd>
          <Show when={c().retired}>
            {(reason) => (
              <>
                <dt>{m().factRetired}</dt>
                <dd>{reason()}</dd>
              </>
            )}
          </Show>
        </dl>
      </section>

      <section class="admin-section" aria-labelledby="ratings-title">
        <h3 id="ratings-title">{m().ratingsTitle(c().ratings.length)}</h3>
        <Show
          when={c().ratings.length > 0}
          fallback={<p class="admin-empty">{m().ratingsEmpty}</p>}
        >
          <ul class="admin-list">
            <For each={c().ratings}>
              {(r) => (
                <li>
                  <span>{m().ratingLine(r.user, r.score, r.judge)}</span>
                  <time datetime={r.at}>{m().when(r.at)}</time>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </section>

      <section class="admin-section" aria-labelledby="runs-title">
        <h3 id="runs-title">{m().runsTitle}</h3>
        <Show when={c().runs.length > 0} fallback={<p class="admin-empty">{m().runsEmpty}</p>}>
          <For each={c().runs}>{(run) => <RunView ctx={props.ctx} run={run} />}</For>
        </Show>
      </section>
    </>
  );
}

/** A run: its outcome and length, then each billed request. */
function RunView(props: { ctx: AdminContext; run: Run }) {
  const m = () => props.ctx.m;
  const total = () => props.run.charges.reduce((sum, c) => sum + c.usd, 0);
  const estimated = () => props.run.charges.some((c) => c.estimated);
  return (
    <div class="admin-run">
      <p class="admin-run-head">
        <span>
          {m().runLine(
            props.run.outcome,
            props.run.elapsed_ms,
            props.run.retried,
            props.run.tool_round,
          )}
        </span>
        <strong>{m().usd(total(), estimated())}</strong>
      </p>
      <ul class="admin-list">
        <For each={props.run.charges}>
          {(charge) => (
            <li>
              <span>
                <strong>{m().stageName[charge.stage]}</strong> · {charge.provider}/{charge.model}
              </span>
              <span>
                {m().tokens(charge.input_tokens, charge.output_tokens)} ·{" "}
                {m().usd(charge.usd, charge.estimated)}
              </span>
            </li>
          )}
        </For>
      </ul>
    </div>
  );
}
