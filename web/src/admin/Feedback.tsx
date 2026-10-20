// /admin/feedback: the feedback queue (#27, docs/DECISIONS.md D38). Every
// Partially correct or Incorrect rating given a reason, newest first, with
// the filters the queue takes. /admin/feedback/<id>: one item, its reason
// beside the call's full material, and the form that closes it.

import { createResource, createSignal, For, Match, Show, Switch } from "solid-js";
import { failed, type Interface, succeeded } from "../api";
import { BotMessage, Message, Notice, PersonAvatar, Typing } from "../Chat";
import {
  type AdminQueueFilter,
  FEEDBACK_CATEGORIES,
  href,
  navigate,
  QUEUE_STATUSES,
  RATER_KINDS,
} from "../router";
import { type AdminContext, ReadFailure } from "./Admin";
import { closeItem, fetchItem, fetchQueue, type ItemView, type QueueItem, type Read } from "./api";
import { CallDetailView } from "./Call";

const INTERFACES: readonly Interface[] = ["discord", "web", "mcp", "cli", "agent_session", "agent"];

export function FeedbackPage(props: { ctx: AdminContext; filter: AdminQueueFilter }) {
  const m = () => props.ctx.m;
  const [items, setItems] = createSignal<QueueItem[]>([]);
  const [next, setNext] = createSignal<string | null>(null);
  const [failure, setFailure] = createSignal<Read<unknown> | null>(null);
  const [loading, setLoading] = createSignal(false);
  let latest = 0;

  async function load(cursor?: string) {
    const mine = ++latest;
    if (cursor === undefined) {
      setItems([]);
      setNext(null);
      setFailure(null);
    }
    setLoading(true);
    const page = await fetchQueue(props.filter, cursor, props.ctx.token, m());
    if (mine !== latest) return;
    setLoading(false);
    if (page.ok) {
      setItems((had) => (cursor === undefined ? page.value.items : [...had, ...page.value.items]));
      setNext(page.value.next);
      setFailure(null);
    } else {
      setFailure(page);
    }
  }
  const [first] = createResource(
    () => ({ ...props.filter }),
    () => load(),
  );

  return (
    <section class="page">
      <header class="page-head">
        <div>
          <h2>{m().feedbackTitle}</h2>
          <p class="page-intro">{m().feedbackIntro}</p>
        </div>
      </header>
      <Filters ctx={props.ctx} filter={props.filter} />
      <Show when={!first.loading} fallback={<Typing m={props.ctx.pub} loading={true} />}>
        <Show
          when={items().length > 0}
          fallback={
            // A page whose every item the server left out (values it could
            // not read) is empty but has a next: not "nothing here".
            <Show when={failure() === null && !next()}>
              <p class="admin-empty">{m().feedbackEmpty}</p>
            </Show>
          }
        >
          <ul class="rows">
            <For each={items()}>
              {(item) => (
                <li>
                  <Row ctx={props.ctx} item={item} />
                </li>
              )}
            </For>
          </ul>
        </Show>
      </Show>
      <Show when={failure()}>{(f) => <ReadFailure ctx={props.ctx} read={f()} />}</Show>
      <Show when={next()}>
        {(cursor) => (
          <p class="more">
            <button
              type="button"
              class="button-quiet"
              disabled={loading()}
              onClick={() => void load(cursor())}
            >
              {loading() ? m().loading : m().loadMoreFeedback}
            </button>
          </p>
        )}
      </Show>
    </section>
  );
}

/** The filters, as a form whose submission is a new address. */
function Filters(props: { ctx: AdminContext; filter: AdminQueueFilter }) {
  const m = () => props.ctx.m;
  let form: HTMLFormElement | undefined;
  const value = (name: string) => {
    const v = form?.elements.namedItem(name);
    return v instanceof HTMLSelectElement ? v.value : "";
  };
  return (
    <form
      ref={form}
      class="admin-filters"
      onSubmit={(e) => {
        e.preventDefault();
        navigate(
          href({
            page: "admin",
            view: {
              view: "feedback",
              filter: {
                status: QUEUE_STATUSES.find((s) => s === value("status")),
                category: FEEDBACK_CATEGORIES.find((c) => c === value("category")),
                interface: INTERFACES.find((i) => i === value("interface")),
                rater: RATER_KINDS.find((k) => k === value("rater")),
              },
            },
          }),
        );
      }}
    >
      <label class="admin-field">
        <span>{m().filterStatus}</span>
        <select name="status" value={props.filter.status ?? ""}>
          <option value="">{m().any}</option>
          <For each={QUEUE_STATUSES}>{(s) => <option value={s}>{m().statusName[s]}</option>}</For>
        </select>
      </label>
      <label class="admin-field">
        <span>{m().filterCategory}</span>
        <select name="category" value={props.filter.category ?? ""}>
          <option value="">{m().any}</option>
          <For each={FEEDBACK_CATEGORIES}>
            {(c) => <option value={c}>{m().categoryName[c]}</option>}
          </For>
        </select>
      </label>
      <label class="admin-field">
        <span>{m().filterInterface}</span>
        <select name="interface" value={props.filter.interface ?? ""}>
          <option value="">{m().any}</option>
          <For each={INTERFACES}>{(i) => <option value={i}>{m().interfaceName[i]}</option>}</For>
        </select>
      </label>
      <label class="admin-field">
        <span>{m().filterRater}</span>
        <select name="rater" value={props.filter.rater ?? ""}>
          <option value="">{m().any}</option>
          <For each={RATER_KINDS}>{(k) => <option value={k}>{m().raterName[k]}</option>}</For>
        </select>
      </label>
      <p class="admin-actions">
        <button type="submit" class="chip chip-action">
          {m().applyFilters}
        </button>
        <a class="chip" href={href({ page: "admin", view: { view: "feedback", filter: {} } })}>
          {m().clearFilters}
        </a>
      </p>
    </form>
  );
}

/** One item as a line linking to its page: who, the question, then the
 * problem, the rating, the state and when. */
function Row(props: { ctx: AdminContext; item: QueueItem }) {
  const m = () => props.ctx.m;
  const i = () => props.item;
  return (
    <a class="row admin-row" href={href({ page: "admin", view: { view: "item", id: i().id } })}>
      <span class="row-line">
        <PersonAvatar name={i().rater_name} />
        <span class="row-question">{i().question}</span>
        <span class="row-facts">
          <span class={`tag-status tag-status-${i().status}`}>{m().statusName[i().status]}</span>
          <span class="badge">
            {i().category === null ? m().noCategory : m().categoryName[i().category ?? "other"]}
          </span>
          <span>{m().scoreName[i().score]}</span>
          <span>{m().raterName[i().rater_kind]}</span>
          <span>{m().interfaceName[i().interface]}</span>
          <time datetime={i().opened_at}>{m().when(i().opened_at)}</time>
        </span>
      </span>
    </a>
  );
}

export function ItemPage(props: { ctx: AdminContext; id: string }) {
  const m = () => props.ctx.m;
  const [read, { mutate }] = createResource(
    () => props.id,
    (id) => fetchItem(id, props.ctx.token, m()),
  );
  // Only onto the item it closed: a reply that lands once the page shows
  // another item is not that item's.
  const closed = (item: QueueItem) => {
    const r = read();
    if (r?.ok && r.value.item.id === item.id) mutate({ ok: true, value: { ...r.value, item } });
  };
  return (
    <section class="page">
      <p class="back">
        <a href={href({ page: "admin", view: { view: "feedback", filter: { status: "open" } } })}>
          <span aria-hidden="true">←</span>
          {m().backToFeedback}
        </a>
      </p>
      <Switch fallback={<Typing m={props.ctx.pub} loading={true} />}>
        {/* Keyed on the item's id, not the value: closing the item replaces
            the value and its close form keeps its confirmation, while another
            item (a jump through history) starts with a fresh form. */}
        <Match when={succeeded(read())}>
          {(view) => (
            <Show when={view().item.id} keyed>
              {(_id) => <Item ctx={props.ctx} view={view()} closed={closed} />}
            </Show>
          )}
        </Match>
        <Match when={failed(read())}>
          {(f) => (
            <Show when={f().status === 404} fallback={<ReadFailure ctx={props.ctx} read={f()} />}>
              <BotMessage m={props.ctx.pub}>
                <Notice tone="note" title={m().itemMissingTitle}>
                  <p>{f().message}</p>
                </Notice>
              </BotMessage>
            </Show>
          )}
        </Match>
      </Switch>
    </section>
  );
}

/** The rater's reason as their message, the item's facts, the form that
 * closes it, then the call whole. */
function Item(props: { ctx: AdminContext; view: ItemView; closed: (item: QueueItem) => void }) {
  const m = () => props.ctx.m;
  const i = () => props.view.item;
  return (
    <>
      <article class="exchange">
        <Message
          class="msg-asker"
          avatar={<PersonAvatar name={i().rater_name} />}
          name={m().raterText(i().rater_kind, i().rater, i().rater_name)}
          header={
            <>
              <span class="badge">
                {i().category === null ? m().noCategory : m().categoryName[i().category ?? "other"]}
              </span>
              <Show when={i().judge}>
                <span class="tag-private">{m().judgeTag}</span>
              </Show>
              <time class="msg-time" datetime={i().rated_at}>
                {m().when(i().rated_at)}
              </time>
            </>
          }
        >
          <div class="bubble bubble-asker admin-reason">
            <p>
              <strong>{m().scoreName[i().score]}</strong>
            </p>
            <Show when={i().wrong}>
              {(text) => (
                <>
                  <h4>{m().reasonWrong}</h4>
                  <p>{text()}</p>
                </>
              )}
            </Show>
            <Show when={i().right}>
              {(text) => (
                <>
                  <h4>{m().reasonRight}</h4>
                  <p>{text()}</p>
                </>
              )}
            </Show>
            <Show when={!i().wrong && !i().right}>
              <p class="admin-note">{m().reasonNoText}</p>
            </Show>
          </div>
        </Message>
      </article>

      <section class="admin-section" aria-labelledby="item-title">
        <h3 id="item-title">{m().itemTitle}</h3>
        <dl class="admin-facts">
          <dt>{m().factStatus}</dt>
          <dd>
            <span class={`tag-status tag-status-${i().status}`}>{m().statusName[i().status]}</span>
          </dd>
          <dt>{m().factRating}</dt>
          <dd>{m().scoreName[i().score]}</dd>
          <dt>{m().factRater}</dt>
          <dd>{m().raterText(i().rater_kind, i().rater, i().rater_name)}</dd>
          <dt>{m().factOpened}</dt>
          <dd>
            <time datetime={i().opened_at}>{m().when(i().opened_at)}</time>
          </dd>
          <Show when={i().resolved_at}>
            {(at) => (
              <>
                <dt>{m().factClosed}</dt>
                <dd>
                  <time datetime={at()}>{m().when(at())}</time>
                </dd>
              </>
            )}
          </Show>
          <Show when={i().note}>
            {(note) => (
              <>
                <dt>{m().factNote}</dt>
                <dd>{note()}</dd>
              </>
            )}
          </Show>
          <Show when={i().fixed_by}>
            {(fix) => (
              <>
                <dt>{m().factFixedBy}</dt>
                <dd>{fix()}</dd>
              </>
            )}
          </Show>
          <Show when={i().ip}>
            {(ip) => (
              <>
                <dt>{m().factAddress}</dt>
                <dd>
                  <code>{ip()}</code>
                </dd>
              </>
            )}
          </Show>
          <Show when={i().user_agent}>
            {(ua) => (
              <>
                <dt>{m().factUserAgent}</dt>
                <dd>{ua()}</dd>
              </>
            )}
          </Show>
        </dl>
        <p>
          <a href={href({ page: "admin", view: { view: "call", id: i().call } })}>{m().openCall}</a>
        </p>
      </section>

      <CloseForm ctx={props.ctx} item={i()} closed={props.closed} />

      <CallDetailView ctx={props.ctx} call={props.view.call} />
    </>
  );
}

/** Resolve or dismiss, with a note and what fixed it. */
function CloseForm(props: {
  ctx: AdminContext;
  item: QueueItem;
  closed: (item: QueueItem) => void;
}) {
  const m = () => props.ctx.m;
  const [saving, setSaving] = createSignal(false);
  const [done, setDone] = createSignal<string | null>(null);
  const [failure, setFailure] = createSignal<Read<unknown> | null>(null);
  let note: HTMLTextAreaElement | undefined;
  let fixedBy: HTMLInputElement | undefined;

  async function close(closing: "resolve" | "dismiss") {
    const text = note?.value.trim() ?? "";
    if (text === "") {
      note?.focus();
      note?.reportValidity();
      return;
    }
    setSaving(true);
    setDone(null);
    setFailure(null);
    const fix = fixedBy?.value.trim() ?? "";
    const r = await closeItem(
      props.item.id,
      closing,
      fix === "" ? { note: text } : { note: text, fixed_by: fix },
      props.ctx.token,
      m(),
    );
    setSaving(false);
    if (r.ok) {
      setDone(m().closed(r.value.status));
      props.closed(r.value);
    } else {
      setFailure(r);
    }
  }

  return (
    <section class="admin-section" aria-labelledby="close-title">
      <h3 id="close-title">{m().closeTitle}</h3>
      <p class="admin-note">{m().closeIntro}</p>
      <form
        class="admin-filters admin-close"
        onSubmit={(e) => {
          e.preventDefault();
          void close("resolve");
        }}
      >
        <label class="admin-field admin-field-wide">
          <span>{m().closeNote}</span>
          <textarea
            ref={note}
            name="note"
            required
            rows={3}
            maxLength={2000}
            placeholder={m().closeNotePlaceholder}
            value={props.item.note ?? ""}
          />
        </label>
        <label class="admin-field admin-field-wide">
          <span>{m().closeFixedBy}</span>
          <input
            ref={fixedBy}
            name="fixed_by"
            maxLength={200}
            autocomplete="off"
            placeholder={m().closeFixedByPlaceholder}
            value={props.item.fixed_by ?? ""}
          />
        </label>
        <p class="admin-actions">
          <button type="submit" class="chip chip-action" disabled={saving()}>
            {saving() ? m().saving : m().resolve}
          </button>
          <button
            type="button"
            class="chip"
            disabled={saving()}
            onClick={() => void close("dismiss")}
          >
            {m().dismiss}
          </button>
          <Show when={done()}>
            {(text) => (
              <span class="admin-note" role="status">
                {text()}
              </span>
            )}
          </Show>
        </p>
      </form>
      <Show when={failure()}>{(f) => <ReadFailure ctx={props.ctx} read={f()} />}</Show>
    </section>
  );
}
