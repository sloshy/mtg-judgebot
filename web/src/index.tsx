/* @refresh reload */
import { render } from "solid-js/web";
// First, so the chosen theme is on <html> before anything draws.
import "./theme";
import App from "./App";
import { isAdminPath } from "./router";
import "./styles.css";

const root = document.getElementById("root");
if (root) {
  // The admin pages (served by the --admin listener only) are their own
  // chunk, loaded only there.
  if (isAdminPath(window.location.pathname)) {
    void import("./admin/Admin").then(({ default: Admin }) => render(() => <Admin />, root));
  } else {
    render(() => <App />, root);
  }
}
