// A modal dialog on the native <dialog> element: it traps focus, closes on
// Escape, and gives focus back to whatever opened it.
import { useEffect, useRef, type ReactNode } from "react";

interface Props {
  title: string;
  onClose: () => void;
  children: ReactNode;
}

export default function Dialog({ title, onClose, children }: Props) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const opener = document.activeElement as HTMLElement | null;
    ref.current?.showModal();
    return () => opener?.focus();
  }, []);
  return (
    <dialog
      ref={ref}
      className="dialog"
      aria-label={title}
      onCancel={(e) => {
        e.preventDefault();
        onClose();
      }}
      onClick={(e) => e.target === ref.current && onClose()}
    >
      <div className="dialog-body">
        <h2>{title}</h2>
        {children}
      </div>
    </dialog>
  );
}
