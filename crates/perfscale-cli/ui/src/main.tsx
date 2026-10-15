import React from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./index.css";

// Marker for the CLI-side test that the embedded UI is the React build.
declare global {
  interface Window {
    __PERFSCALE_UI__?: string;
  }
}
window.__PERFSCALE_UI__ = "react";

createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
