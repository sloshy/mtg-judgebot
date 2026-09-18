# web

The anonymous web front end: one SolidJS page, built with Vite and served by
`judge-api --web` from `WEB_DIST`. The Docker image builds it into `/srv/web`, and the
compose file passes `--api --web`. Without the flag the page is not served. It calls
`POST /api/judge`, renders the answer with its citations and the "did you mean…?"
choices, and draws mana and card symbols with Scryfall's SVGs (`src/Symbols.tsx`).
There are no accounts and no rating buttons here. Ratings are a Discord feature.

```sh
npm ci
npm run dev      # Vite on :5173, proxying /api to a local `cargo run -p judge-api` on :8787
npm run build    # tsc + vite build → dist/
```

The page keeps a random client id in `sessionStorage`. Follow-up questions in one
browser tab share a history (`web:<uuid>` thread ids on the server), and closing the tab
ends it. Nothing else is stored client-side.
