# web

The anonymous web front end: one SolidJS page, built with Vite and served by
`judgebot --web` from `WEB_DIST`. The Docker image builds it into `/srv/web`, and the
compose file passes `--api --web`. Without the flag the page is not served. It calls
`POST /api/judge` and renders the conversation (`src/App.tsx`, `src/Chat.tsx`,
`src/Answer.tsx`), and reads the public feed (`src/Feed.tsx`: `/feed`, `/calls/<id>`,
`/users/<name>`, routed client-side by `src/router.ts`). It draws mana and card symbols
with Scryfall's SVGs (`src/Symbols.tsx`). Every string is in `src/messages.ts`. The
header's documentation link is `VITE_DOCS_URL` at build time, else the project's site
(`src/links.ts`).
There are no accounts and no rating buttons here. Ratings are a Discord feature.

```sh
npm ci
npm run dev      # Vite on :5173, proxying /api to a local `cargo run -p judgebot -- --api --web` on :8787
npm run build    # tsc + vite build → dist/
```

The page keeps a random client id in `sessionStorage`. Follow-up questions in one
browser tab share a history (`web:<uuid>` thread ids on the server), and closing the tab
ends it. `localStorage` keeps two display choices, the theme (`judgebot-theme`) and the
feed's layout; nothing else is stored client-side.
