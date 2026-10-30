// The Timeline view: tracks and clips on the transport's beat grid. Until
// idevkore/dk-tunekore#36 builds it, this is a placeholder with the view
// switch and the window's status line.
import type { ReactNode } from "react";
import logo from "../../brand/tunekore-logo-color.svg";
import mark from "../../brand/tunekore-mark.svg";
import StatusBar, { type Message } from "../StatusBar";

const mod = navigator.platform.toLowerCase().includes("mac") ? "⌘" : "Ctrl+";

type Props = {
  switcher: ReactNode;
  message: Message | null;
  onMessage: (m: Message | null) => void;
};

export default function Timeline({ switcher, message, onMessage }: Props) {
  return (
    <div className="app timeline">
      <header className="bar">
        <img src={logo} alt="tuneKore" className="bar-logo wide-only" />
        <img src={mark} alt="tuneKore" className="bar-mark narrow-only" />
        {switcher}
      </header>
      <main className="timeline-empty">
        <h1>Timeline</h1>
        <p>The Timeline arrives in #36: tracks, audio and MIDI clips and recording, on the same beat clock as the Sampler.</p>
        <p className="dim">
          <kbd>{mod}1</kbd> goes back to the Sampler.
        </p>
      </main>
      <StatusBar message={message} onDismiss={() => onMessage(null)}>
        <kbd>Space</kbd> play · <kbd>{mod}1</kbd> Sampler · <kbd>{mod}S</kbd> save
      </StatusBar>
    </div>
  );
}
