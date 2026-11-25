import { useEffect, useState, type MouseEvent, type Ref } from 'react';
import {
  closeDesktopWindow,
  desktopWindowControlsAvailable,
  dragDesktopWindow,
  dragResizeDesktopWindow,
  getDesktopWindowState,
  minimizeDesktopWindow,
  subscribeDesktopWindowState,
  toggleMaximizeDesktopWindow,
  type DesktopResizeDirection,
} from '../lib/desktop';

const INTERACTIVE_SELECTOR =
  'button, a, input, textarea, select, [role="button"], [contenteditable="true"], .no-drag';

function isInteractiveTarget(target: EventTarget | null): boolean {
  return target instanceof Element && Boolean(target.closest(INTERACTIVE_SELECTOR));
}

export function dragWindowMouseDown(event: MouseEvent<HTMLElement>) {
  if (event.button !== 0 || isInteractiveTarget(event.target)) return;
  dragDesktopWindow();
}

export function dragWindowDoubleClick(event: MouseEvent<HTMLElement>) {
  if (isInteractiveTarget(event.target)) return;
  toggleMaximizeDesktopWindow();
}

function useWindowMaximized() {
  const [maximized, setMaximized] = useState(false);

  useEffect(() => {
    if (!desktopWindowControlsAvailable()) return;
    void getDesktopWindowState()
      .then((s) => setMaximized(Boolean(s?.maximized)))
      .catch(() => {});
    return subscribeDesktopWindowState((s) => setMaximized(Boolean(s?.maximized)));
  }, []);

  return maximized;
}

function WindowControls() {
  const maximized = useWindowMaximized();

  if (!desktopWindowControlsAvailable()) return null;

  return (
    <div className="desktop-window-controls" role="group" aria-label="Window controls">
      <button
        type="button"
        className="desktop-window-btn"
        aria-label="Minimize window"
        title="Minimize"
        onClick={() => minimizeDesktopWindow()}
      >
        <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
          <path d="M1.5 5h7" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" />
        </svg>
      </button>
      <button
        type="button"
        className="desktop-window-btn"
        aria-label={maximized ? 'Restore window' : 'Maximize window'}
        title={maximized ? 'Restore' : 'Maximize'}
        onClick={() => toggleMaximizeDesktopWindow()}
      >
        {maximized ? (
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <rect
              x="1.5"
              y="3"
              width="5.5"
              height="5.5"
              rx="0.7"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.1"
            />
            <path
              d="M3.2 3V2a.8.8 0 0 1 .8-.8h4a.8.8 0 0 1 .8.8v4a.8.8 0 0 1-.8.8H7"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.1"
            />
          </svg>
        ) : (
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <rect
              x="1.5"
              y="1.5"
              width="7"
              height="7"
              rx="0.8"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.15"
            />
          </svg>
        )}
      </button>
      <button
        type="button"
        className="desktop-window-btn is-close"
        aria-label="Close window"
        title="Close"
        onClick={() => closeDesktopWindow()}
      >
        <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
          <path
            d="M2 2l6 6M8 2L2 8"
            stroke="currentColor"
            strokeWidth="1.2"
            strokeLinecap="round"
          />
        </svg>
      </button>
    </div>
  );
}

export function DesktopToolbar({ actionsRef }: { actionsRef?: Ref<HTMLDivElement> }) {
  return (
    <header
      className="desktop-toolbar"
      onMouseDown={dragWindowMouseDown}
      onDoubleClick={dragWindowDoubleClick}
    >
      <div className="desktop-toolbar-actions" ref={actionsRef} />
      <WindowControls />
    </header>
  );
}

export function DesktopTitlebar() {
  return (
    <div
      className="desktop-titlebar"
      onMouseDown={dragWindowMouseDown}
      onDoubleClick={dragWindowDoubleClick}
    >
      <div className="desktop-titlebar-controls">
        <WindowControls />
      </div>
    </div>
  );
}

const resizeDirs: DesktopResizeDirection[] = ['n', 'ne', 'e', 'se', 's', 'sw', 'w', 'nw'];

export function DesktopResizeEdges() {
  const maximized = useWindowMaximized();
  if (!desktopWindowControlsAvailable() || maximized) return null;
  return (
    <>
      {resizeDirs.map((dir) => (
        <div
          key={dir}
          className={`desktop-resize-edge desktop-resize-edge-${dir}`}
          aria-hidden="true"
          onMouseDown={(event) => {
            if (event.button !== 0) return;
            event.preventDefault();
            dragResizeDesktopWindow(dir);
          }}
        />
      ))}
    </>
  );
}
