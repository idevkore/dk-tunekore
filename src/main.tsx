import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import "./App.css";

async function start() {
  // `npm run dev` only: try the window in a browser with ?mock (see sampler/devmock.ts).
  if (import.meta.env.DEV && new URLSearchParams(location.search).has("mock")) {
    (await import("./sampler/devmock")).installMocks();
  }
  createRoot(document.getElementById("root")!).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}

void start();
