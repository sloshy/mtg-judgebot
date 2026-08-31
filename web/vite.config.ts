import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

export default defineConfig({
  plugins: [solid()],
  server: {
    // `npm run dev` proxies API calls to a locally running `cargo run -p judge-api`.
    proxy: { "/api": "http://localhost:8787" },
  },
});
