import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

export default defineConfig({
  plugins: [solid()],
  server: {
    // `npm run dev` proxies API calls to a locally running `cargo run -p judgebot -- --api --web`
    // (and the admin pages' to its `--admin` listener).
    proxy: { "/api": "http://localhost:8787", "/admin/api": "http://localhost:8789" },
  },
});
