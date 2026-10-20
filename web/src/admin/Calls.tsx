// /admin: every stored call, private ones included, newest first, with the
// filters the list takes (interface, asker, model, visibility, text).

import { createResource, createSignal, For, Show } from "solid-js";
import type { Interface } from "../api";
import { PersonAvatar, Typing } from "../Chat";
import { type AdminCallFilter, href, navigate } from "../router";
import { type AdminContext, ReadFailure } from "./Admin";
import { type CallRow, fetchCalls, type Read } from "./api";

const INTERFACES: readonly Interface[] = ["discord", "web", "mcp", "cli", "agent_session", "agent"];

export function CallsPage(props: { ctx: AdminContext; filter: AdminCallFilter }) {
  const m = () => props.ctx.m;
  const [rows, setRows] = createSignal<CallRow[]>([]);
  const [next, setNext] = createSignal<string | null>(null);
  const [models, setModels] = createSignal<string[]>([]);
  const [failure, setFailure] = createSignal<Read<unknown> | null>(null);
  const [loading, setLoading] = createSignal(false);
  let latest = 0;

  async function load(cursor?: string) {
    const mine = ++latest;
    if (cursor === undefined) {
      setRows([]);
      setNext(null);
      setFailure(null);
    }
    setLoading(true);
    const page = await fetchCalls(props.filter, cursor, props.ctx.token, m());
    if (mine !== latest) return;
    setLoading(false);
    if (page.ok) {
      setRows((had) => (cursor === undefined ? page.value.calls : [...had, ...page.value.calls]));
      setNext(page.value.next);
      setModels(page.value.models);
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
          <h2>{m().callsTitle}</h2>
          <p class="page-intro">{m().callsIntro}</p>
        </div>
      </header>
      <Filters ctx={props.ctx} filter={props.filter} models={models()} />
      <Show when={!first.loading} fallback={<Typing m={props.ctx.pub} loading={true} />}>
        <Show
          when={rows().length > 0}
          fallback={
            <Show when={failure() === null}>
              <p class="admin-empty">{m().callsEmpty}</p>
            </Show>
          }
        >
          <ul class="rows">
            <For each={rows()}>
              {(row) => (
                <li>
                  <Row ctx={props.ctx} row={row} />
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
              {loading() ? m().loading : m().loadMore}
            </button>
          </p>
        )}
      </Show>
    </section>
  );
}

/** The filters, as a form whose submission is a new address: a filtered
 * list is a link like any other. */
function Filters(props: { ctx: AdminContext; filter: AdminCallFilter; models: string[] }) {
  const m = () => props.ctx.m;
  let form: HTMLFormElement | undefined;
  const value = (name: string) => {
    const v = form?.elements.namedItem(name);
    return v instanceof HTMLInputElement || v instanceof HTMLSelectElement ? v.value.trim() : "";
  };
  return (
    <form
      ref={form}
      class="admin-filters"
      onSubmit={(e) => {
        e.preventDefault();
        const asked = value("interface");
        const visibility = value("private");
        navigate(
          href({
            page: "admin",
            view: {
              view: "calls",
              filter: {
                interface: INTERFACES.find((i) => i === asked),
                asker: value("asker") || undefined,
                model: value("model") || undefined,
                private: visibility === "true" ? true : visibility === "false" ? false : undefined,
                q: value("q") || undefined,
              },
            },
          }),
        );
      }}
    >
      <label class="admin-field admin-field-wide">
        <span>{m().filterSearch}</span>
        <input
          name="q"
          type="search"
          value={props.filter.q ?? ""}
          placeholder={m().filterSearchPlaceholder}
          maxLength={100}
          autocomplete="off"
        />
      </label>
      <label class="admin-field">
        <span>{m().filterInterface}</span>
        <select name="interface" value={props.filter.interface ?? ""}>
          <option value="">{m().any}</option>
          <For each={INTERFACES}>{(i) => <option value={i}>{m().interfaceName[i]}</option>}</For>
        </select>
      </label>
      <label class="admin-field">
        <span>{m().filterAsker}</span>
        <input
          name="asker"
          value={props.filter.asker ?? ""}
          placeholder={m().filterAskerPlaceholder}
          maxLength={100}
          autocomplete="off"
        />
      </label>
      <label class="admin-field">
        <span>{m().filterModel}</span>
        <select name="model" value={props.filter.model ?? ""}>
          <option value="">{m().any}</option>
          <For each={props.models}>{(model) => <option value={model}>{model}</option>}</For>
          <Show when={props.filter.model && !props.models.includes(props.filter.model)}>
            <option value={props.filter.model}>{props.filter.model}</option>
          </Show>
        </select>
      </label>
      <label class="admin-field">
        <span>{m().filterVisibility}</span>
        <select
          name="private"
          value={props.filter.private === undefined ? "" : String(props.filter.private)}
        >
          <option value="">{m().any}</option>
          <option value="true">{m().onlyPrivate}</option>
          <option value="false">{m().onlyPublic}</option>
        </select>
      </label>
      <p class="admin-actions">
        <button type="submit" class="chip chip-action">
          {m().applyFilters}
        </button>
        <a class="chip" href={href({ page: "admin", view: { view: "calls", filter: {} } })}>
          {m().clearFilters}
        </a>
      </p>
    </form>
  );
}

/** Who asked: the username, else the Discord id, else anonymous. */
export function askerText(m: AdminContext["m"], asker: string | null, id: string | null): string {
  if (asker !== null) return `@${asker}`;
  if (id !== null) return m.askerId(id);
  return m.anonymous;
}

/** One call as a line linking to its page: who, the question, then where,
 * when, its visibility, its models and its cost. */
function Row(props: { ctx: AdminContext; row: CallRow }) {
  const m = () => props.ctx.m;
  return (
    <a
      class="row admin-row"
      href={href({ page: "admin", view: { view: "call", id: props.row.id } })}
    >
      <span class="row-line">
        <PersonAvatar name={props.row.asker} />
        <span class="row-question">{props.row.question}</span>
        <span class="row-facts">
          <span class="badge">{m().interfaceName[props.row.interface]}</span>
          <Show when={props.row.private}>
            <span class="tag-private">{m().privateTag}</span>
          </Show>
          <Show when={props.row.retired}>
            <span class="tag-retired">{m().retiredTag}</span>
          </Show>
          <span>{askerText(m(), props.row.asker, props.row.asker_id)}</span>
          <time datetime={props.row.asked_at}>{m().when(props.row.asked_at)}</time>
          <span>
            {props.row.usd === null ? m().noRun : m().usd(props.row.usd, props.row.cost_estimated)}
          </span>
          <Show when={props.row.models.length > 0}>
            <span class="admin-models">{props.row.models.join(", ")}</span>
          </Show>
        </span>
      </span>
    </a>
  );
}
