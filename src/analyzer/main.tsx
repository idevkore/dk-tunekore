import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import Analyzer from "./Analyzer";
import "./analyzer.css";

async function start() {
  // `npm run dev` only: try the window in a browser with ?mock (see devmock.ts).
  if (import.meta.env.DEV && new URLSearchParams(location.search).has("mock")) {
    (await import("./devmock")).installMocks();
  }
  createRoot(document.getElementById("root")!).render(
    <StrictMode>
      <Analyzer />
    </StrictMode>,
  );
}

void start();
