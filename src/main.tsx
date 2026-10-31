import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import "./App.css";
import "./theme/themes.css";
import { applyTheme, storedTheme } from "./theme/theme";

async function start() {
  // The saved colour theme, before the first paint.
  applyTheme(storedTheme().id, false);
  // `npm run dev` only: try the window in a browser with ?mock (see sampler/devmock.ts).
  if (import.meta.env.DEV && new URLSearchParams(location.search).has("mock")) {
    (await import("./sampler/devmock")).installMocks();
    (await import("./timeline/devmock")).installMocks();
    (await import("./plugins/devmock")).installMocks();
  }
  createRoot(document.getElementById("root")!).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}

void start();
