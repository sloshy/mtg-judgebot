// The pieces every page's messages are built from: the judge's avatar and
// message, a person's avatar and message, and the typing indicator.

import type { JSX } from "solid-js";
import { Show } from "solid-js";
import type { Messages } from "./messages";

/** The judge's icon: 32x32 pixel art, shown at a whole multiple of 32. */
export function BotAvatar(props: { size?: 32 | 64 }) {
  const size = () => props.size ?? 32;
  return (
    <img
      class="avatar avatar-bot"
      src="/favicon.png"
      alt=""
      width={size()}
      height={size()}
      style={{ width: `${size()}px`, height: `${size()}px` }}
    />
  );
}

/** A hue from a name, so one asker keeps one colour across the feed. */
function hue(name: string): number {
  let h = 0;
  for (const ch of name) h = (h * 31 + (ch.codePointAt(0) ?? 0)) % 360;
  return h;
}

/** An asker's avatar: their initial on a colour of their own, or a plain
 * figure for an anonymous question. */
export function PersonAvatar(props: { name: string | null; large?: boolean }) {
  return (
    <span
      class="avatar avatar-person"
      classList={{ "avatar-large": props.large === true, "avatar-anon": props.name === null }}
      style={props.name === null ? undefined : { "--hue": String(hue(props.name)) }}
      aria-hidden="true"
    >
      <Show when={props.name} fallback={<PersonGlyph />}>
        {(name) => Array.from(name())[0]?.toUpperCase()}
      </Show>
    </span>
  );
}

function PersonGlyph() {
  return (
    <svg viewBox="0 0 20 20" width="18" height="18" aria-hidden="true">
      <circle cx="10" cy="7" r="3.5" fill="currentColor" />
      <path d="M3.5 17.5c.8-3.6 3.4-5.5 6.5-5.5s5.7 1.9 6.5 5.5z" fill="currentColor" />
    </svg>
  );
}

/** One message in a thread: avatar, a header line, then the body. */
export function Message(props: {
  avatar: JSX.Element;
  name: JSX.Element;
  header?: JSX.Element;
  children: JSX.Element;
  class?: string;
}) {
  return (
    <div class={`msg ${props.class ?? ""}`}>
      <div class="msg-avatar">{props.avatar}</div>
      <p class="msg-head">
        <span class="msg-name">{props.name}</span>
        {props.header}
      </p>
      <div class="msg-body">{props.children}</div>
    </div>
  );
}

/** A message from the judge. */
export function BotMessage(props: { m: Messages; header?: JSX.Element; children: JSX.Element }) {
  return (
    <Message class="msg-bot" avatar={<BotAvatar />} name={props.m.title} header={props.header}>
      {props.children}
    </Message>
  );
}

/** The judge at work: three dots and how long it usually takes (or, for a
 * page being read rather than an answer being written, that it is loading). */
export function Typing(props: { m: Messages; loading?: boolean }) {
  return (
    <BotMessage m={props.m}>
      <div class="bubble bubble-bot typing" role="status">
        <span class="dots" aria-hidden="true">
          <span />
          <span />
          <span />
        </span>
        <span>{props.loading === true ? props.m.loading : props.m.thinking}</span>
      </div>
    </BotMessage>
  );
}

/** An arrow pointing out of the page, after a link that opens a new tab. */
export function OutIcon() {
  return (
    <svg class="out-icon" viewBox="0 0 16 16" width="12" height="12" aria-hidden="true">
      <path
        d="M6 3h7v7M13 3 4 12"
        fill="none"
        stroke="currentColor"
        stroke-width="1.8"
        stroke-linecap="round"
        stroke-linejoin="round"
      />
    </svg>
  );
}

/** How serious a notice is: information, a passing problem, a failure. */
export type Tone = "note" | "caution" | "danger";

/** The judge saying something other than an answer: a titled notice inside
 * its message bubble, so it stays part of the conversation. */
export function Notice(props: {
  tone: Tone;
  title: string;
  /** For a notice that is not already inside a live region. */
  role?: "status" | "alert";
  children: JSX.Element;
}) {
  return (
    <div class={`bubble bubble-bot notice notice-${props.tone}`} role={props.role}>
      <p class="notice-title">
        <ToneIcon tone={props.tone} />
        {props.title}
      </p>
      <div class="notice-body">{props.children}</div>
    </div>
  );
}

function ToneIcon(props: { tone: Tone }) {
  return (
    <svg class="notice-icon" viewBox="0 0 20 20" width="18" height="18" aria-hidden="true">
      <Show
        when={props.tone !== "note"}
        fallback={
          <>
            <circle cx="10" cy="10" r="7.5" fill="none" stroke="currentColor" stroke-width="1.8" />
            <path d="M10 9v4.5" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" />
            <circle cx="10" cy="6.3" r="1.1" fill="currentColor" />
          </>
        }
      >
        <Show
          when={props.tone === "caution"}
          fallback={
            <path
              d="M7 2.5h6L17.5 7v6L13 17.5H7L2.5 13V7z"
              fill="none"
              stroke="currentColor"
              stroke-width="1.8"
              stroke-linejoin="round"
            />
          }
        >
          <path
            d="M10 2.8 18 16.5H2z"
            fill="none"
            stroke="currentColor"
            stroke-width="1.8"
            stroke-linejoin="round"
          />
        </Show>
        <path d="M10 7.5v4" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" />
        <circle cx="10" cy="14" r="1.1" fill="currentColor" />
      </Show>
    </svg>
  );
}
