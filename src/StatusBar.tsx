// The status line at the foot of the main window: the latest message (a
// save, an error) or the view's keyboard hints, and the version. The message
// belongs to the window, so it shows whichever view is on screen.
import type { ReactNode } from "react";

export type Message = { text: string; error: boolean };

export default function StatusBar(props: { message: Message | null; onDismiss: () => void; children: ReactNode }) {
  const { message } = props;
  return (
    <footer className="status" aria-live="polite">
      {message ? (
        <span className={`msg${message.error ? " error" : ""}`}>
          {message.text}
          {message.error && (
            <button className="link" onClick={props.onDismiss}>
              Dismiss
            </button>
          )}
        </span>
      ) : (
        <span className="hints">{props.children}</span>
      )}
      <span className="ver mono" title="tuneKore version">
        v{__APP_VERSION__}
      </span>
    </footer>
  );
}
