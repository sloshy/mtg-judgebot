/* @refresh reload */
import { render } from "solid-js/web";
// First, so the chosen theme is on <html> before anything draws.
import "./theme";
import App from "./App";
import "./styles.css";

const root = document.getElementById("root");
if (root) render(() => <App />, root);
