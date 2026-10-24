import { paletteTerminal } from './palette';
import { call, currentMode, onTransportChange, subscribe, watchTerminal } from './bridge';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { currentTheme } from './theme';
import { textScale } from './ui-size';
import '@xterm/xterm/css/xterm.css';

export interface TerminalContext {
  projectId: string;
  sessionId?: string;
  harness?: { provider: import('./types').ProviderId; command?: string };
  runScript?: boolean;
  runRequestId?: string;
}

export interface TerminalActivity { running: boolean; processNames: string[]; uncertain: boolean }
export interface TerminalController {
  readonly exited: boolean;
  dispose(): Promise<void>;
  activity(): Promise<TerminalActivity>;
  focus(): void;
  offerCommand(command?: string): void;
}
interface TerminalPreferences {
  source?: string;
  fontFamily?: string;
  lineHeight?: number;
  cursorStyle?: 'block' | 'bar' | 'underline';
  cursorBlink?: boolean;
  theme?: import('@xterm/xterm').ITheme;
}

export async function mountTerminal(container: HTMLElement, context: TerminalContext, reportError: (error: unknown) => void): Promise<TerminalController> {
  if (context.harness && !context.sessionId) throw new Error('Select a task before opening its harness console.');
  let command = context.harness?.command;
  let commandRevision = 0;
  const validateCommand = (value?: string) => {
    if (value && (!/^\/(?:[\p{L}\p{N}_:.-]+|\?)(?:\s|$)/u.test(value) || value.length > 4096 || /[\u0000-\u001f\u007f]/.test(value))) throw new Error('Use a single slash command without control characters.');
  };
  validateCommand(command);
  const frame = document.createElement('div');
  frame.style.cssText = 'height:100%;min-height:0;display:flex;flex-direction:column;gap:8px';
  const surface = document.createElement('div');
  surface.style.cssText = 'flex:1;min-height:0;overflow:hidden';
  frame.append(surface); container.append(frame);
  let insertButton: HTMLButtonElement | undefined;
  if (context.harness) {
    const controls = document.createElement('div');
    controls.className = 'terminal-harness-controls';
    controls.style.cssText = 'display:flex;align-items:center;gap:12px;flex-wrap:wrap;flex:none';
    const hint = document.createElement('span');
    hint.textContent = context.harness.provider === 'kimi'
      ? 'Uses Kimi’s saved permissions. Review them in the console before pressing Enter.'
      : command ? 'When the console is ready, insert the command and press Enter.' : 'Native harness console. Type a command to continue.';
    hint.style.cssText = 'color:var(--muted);font-size:.78rem;flex:1';
    controls.append(hint);
    insertButton = document.createElement('button');
    insertButton.type = 'button'; insertButton.className = 'secondary-button';
    insertButton.hidden = !command;
    insertButton.textContent = command ? `Insert ${command.split(/\s/, 1)[0]}` : '';
    insertButton.setAttribute('aria-label', `Insert ${command ?? ''} in the harness console`);
    insertButton.disabled = true;
    controls.append(insertButton);
    frame.prepend(controls);
  }
  const preferences = await call<TerminalPreferences>('get_terminal_preferences').catch(() => ({} as TerminalPreferences));
  if (preferences.source) container.dataset.terminalSettings = preferences.source;
  const theme = () => ({ ...terminalTheme(), ...paletteTerminal(), ...preferences.theme });
  const terminal = new Terminal({ fontFamily: preferences.fontFamily ? `${JSON.stringify(preferences.fontFamily)}, monospace` : '"SF Mono", "Cascadia Code", Menlo, monospace', fontSize: 12 * textScale(), lineHeight: preferences.lineHeight ?? 1.5, cursorStyle: preferences.cursorStyle ?? 'block', cursorBlink: preferences.cursorBlink ?? true, scrollback: 1500, minimumContrastRatio: 4.5, theme: theme() });
  const fit = new FitAddon(); terminal.loadAddon(fit); terminal.open(surface);
  if (surface.clientWidth && surface.clientHeight) fit.fit();
  let terminalId = '', exited = false, hasOutput = false, disposed = false, inserted = false;
  // Each pending PTY owns its startup output. A busy existing tab must not
  // consume another tab's queue before start_terminal returns that tab's ID. Only the
  // local transports need it: their event stream has no ring, so output emitted
  // before the id is known is only catchable live. A gateway replays instead.
  const early = new Map<string, { terminalId: string; data: string; reason?: string; exit: boolean }>();
  /** Writes the line that explains an end, instead of leaving a pane that looks dead. */
  const closeNote = (reason?: string): string => reason === 'idle'
    ? 'The gateway closed this terminal after it sat idle with nobody watching.'
    : 'Process exited. Close and reopen the terminal to start again.';
  const handle = (event: { terminalId: string; data: string; exit?: boolean; reason?: string }) => {
    if (!terminalId) {
      const previous = early.get(event.terminalId);
      early.set(event.terminalId, { terminalId: event.terminalId, data: ((previous?.data ?? '') + event.data).slice(-65536), exit: Boolean(previous?.exit || event.exit), reason: previous?.reason ?? event.reason });
      // Bound each native PTY's output independently and retain its exit marker
      // when its buffer rolls. Other PTYs never evict this terminal's startup.
      return;
    }
    if (event.terminalId !== terminalId || disposed) return;
    terminal.write(event.data); hasOutput ||= event.data.length > 0;
    if (event.exit) { exited = true; terminal.write(`\r\n\x1b[90m${closeNote(event.reason)}\x1b[0m\r\n`); }
    if (insertButton) insertButton.disabled = !hasOutput || exited || inserted;
  };
  /**
   * The server could not replay this console that far back. A shell cannot be
   * refetched the way a turn's transcript can (that is why a gateway sends this
   * instead of `lagged`), so the pane is cleared and the reason is written before
   * the frames it did keep are redrawn over it.
   */
  const onReset = () => {
    if (disposed) return;
    terminal.clear();
    terminal.write('\x1b[90mThe console lost its earlier scrollback and was reset. Showing what the gateway kept.\x1b[0m\r\n');
    hasOutput = true;
  };
  let unlisten: () => void = () => {};
  let localSubscribed = false;
  /** The shared event stream, for a backend that has no ring to replay from. */
  const bindLocal = async (): Promise<void> => {
    if (localSubscribed) return;
    unlisten = await subscribe('terminal-event', handle);
    localSubscribed = true;
  };
  try {
    // Local transports subscribe before the PTY exists so no startup output is missed.
    // Remote opens its own filtered socket once the id is known (see begin()).
    if (currentMode() !== 'remote') await bindLocal();
  } catch (error) { terminal.dispose(); frame.remove(); throw error; }
  let stopWatch: (() => void) | undefined;
  const watch = async (id: string): Promise<void> => {
    stopWatch?.(); stopWatch = undefined;
    try {
      const off = await watchTerminal(id, handle, onReset);
      // A restart may have moved on while the socket opened: drop this one instead.
      if (disposed || terminalId !== id) { off(); return; }
      stopWatch = off;
    } catch (error) { if (!disposed) reportError(error); }
  };
  /** Starts a PTY on the transport that is active now, then watches it. */
  const begin = async (): Promise<void> => {
    const { harness, runScript, runRequestId, ...workspace } = context;
    const id = await call<string>(runScript ? 'start_run_script_terminal' : harness ? 'start_harness_terminal' : 'start_terminal', { ...workspace, ...(runRequestId ? { requestId: runRequestId } : {}), cols: terminal.cols, rows: terminal.rows });
    if (disposed) { await call('close_terminal', { terminalId: id }).catch(() => undefined); return; }
    terminalId = id;
    const pending = early.get(id); early.clear(); if (pending) handle(pending);
    // Remote: the filtered socket (since=0) is both the scrollback this console has
    // never seen and the gateway's record that somebody is watching it. Local: the
    // shared stream, which a console that arrived here from the gateway has not
    // subscribed to yet.
    if (currentMode() === 'remote') { unlisten(); localSubscribed = false; await watch(id); }
    else await bindLocal();
    // The pane may have been resized while start_terminal was in flight; the shell
    // was told the size it had then.
    resize();
  };
  /**
   * The transport changed under this console. Its PTY belongs to the backend that
   * started it — the id is meaningless on the other side — so the shell is closed
   * there and a new one is started here, saying so rather than leaving a pane that
   * accepts keystrokes into nothing.
   */
  const restart = async (): Promise<void> => {
    if (disposed) return;
    const previous = terminalId;
    stopWatch?.(); stopWatch = undefined;
    unlisten(); localSubscribed = false;
    terminalId = ''; exited = false; hasOutput = false; inserted = false;
    terminal.reset();
    terminal.write(`\x1b[90m${currentMode() === 'remote' ? 'Moved to the gateway.' : 'Back on this computer.'} The console restarted; this shell is new.\x1b[0m\r\n`);
    if (insertButton) insertButton.disabled = true;
    const closing = previous ? call('close_terminal', { terminalId: previous }).catch(() => undefined) : Promise.resolve();
    try { await closing; await begin(); } catch (error) { if (!disposed) reportError(error); }
  };
  const unwatchTransport = onTransportChange(() => { void restart(); });
  if (insertButton) insertButton.addEventListener('click', () => {
    if (exited || disposed || !hasOutput || !command) return;
    const revision = commandRevision, text = command;
    insertButton!.disabled = true; inserted = true;
    // Paste into the visible TUI composer only. Never send Enter automatically:
    // authentication, workspace trust or onboarding may still be on screen.
    void call('write_terminal', { terminalId, data: `\x1b[200~${text}\x1b[201~` }).then(() => {
      if (!disposed && revision === commandRevision) { insertButton!.textContent = 'Inserted · press Enter'; focus(); }
    }).catch(error => {
      if (!disposed && revision === commandRevision) { inserted = false; insertButton!.disabled = exited || !hasOutput; }
      reportError(error);
    });
  });
  const refreshTheme = () => { terminal.options.theme = theme(); };
  refreshTheme();
  window.addEventListener('pandacode-theme-change', refreshTheme);
  window.addEventListener('pandacode-palette-change', refreshTheme);
  const input = terminal.onData(data => { if (!exited && !disposed && terminalId) void call('write_terminal', { terminalId, data }).catch(reportError); });
  let resizeFrame = 0;
  // Before the PTY exists there is nobody to resize: `fit` still measures the pane,
  // and begin() sends the size it settled on as soon as the shell is there.
  const resize = () => { cancelAnimationFrame(resizeFrame); resizeFrame = requestAnimationFrame(() => { if (disposed || !surface.clientWidth || !surface.clientHeight) return; fit.fit(); if (!terminalId || exited) return; void call('resize_terminal', { terminalId, cols: terminal.cols, rows: terminal.rows }).catch(reportError); }); };
  const refreshSize = () => { terminal.options.fontSize = 12 * textScale(); resize(); };
  refreshSize();
  window.addEventListener('pandacode-ui-scale-change', refreshSize);
  const observer = new ResizeObserver(resize);
  observer.observe(surface);
  try {
    await begin();
  } catch (error) {
    unlisten(); stopWatch?.(); unwatchTransport(); observer.disconnect();
    window.removeEventListener('pandacode-theme-change', refreshTheme); window.removeEventListener('pandacode-palette-change', refreshTheme); window.removeEventListener('pandacode-ui-scale-change', refreshSize);
    cancelAnimationFrame(resizeFrame); input.dispose(); terminal.dispose(); frame.remove(); throw error;
  }
  const focus = () => { if (!disposed && surface.clientWidth && surface.clientHeight) { resize(); terminal.focus(); } };
  focus();
  return { get exited() { return exited; }, focus, offerCommand: value => {
    validateCommand(value);
    if (!insertButton || !value) return;
    ++commandRevision; command = value; inserted = false; insertButton.hidden = false;
    insertButton.textContent = `Insert ${value.split(/\s/, 1)[0]}`;
    insertButton.setAttribute('aria-label', `Insert ${value} in the harness console`);
    insertButton.disabled = exited || disposed || !hasOutput;
  }, activity: () => exited || disposed || !terminalId ? Promise.resolve({ running: false, processNames: [], uncertain: false }) : call<TerminalActivity>('get_terminal_activity', { terminalId }), dispose: async () => {
    if (disposed) return; disposed = true;
    window.removeEventListener('pandacode-theme-change', refreshTheme); window.removeEventListener('pandacode-palette-change', refreshTheme); window.removeEventListener('pandacode-ui-scale-change', refreshSize); observer.disconnect(); cancelAnimationFrame(resizeFrame); input.dispose(); unlisten(); stopWatch?.(); stopWatch = undefined; unwatchTransport(); terminal.dispose(); frame.remove();
    // The console is gone, so a shell that the transport has already dropped is not an error.
    if (terminalId) await call('close_terminal', { terminalId }).catch(() => undefined);
  } };
}

function terminalTheme() {
  return currentTheme() === 'dark'
    ? { background: '#191a18', foreground: '#efefeb', cursor: '#ffd43b', cursorAccent: '#191a18', selectionBackground: '#ffd43b40', black: '#32342f', red: '#f28b82', green: '#96ce9a', yellow: '#e8ca70', blue: '#8eb5f2', magenta: '#cda1dc', cyan: '#88cbd1', white: '#d5d6ce', brightBlack: '#96998f', brightRed: '#ffaba3', brightGreen: '#b0e4b3', brightYellow: '#ffe39b', brightBlue: '#accbff', brightMagenta: '#e4bceF', brightCyan: '#a6e6ec', brightWhite: '#f6f6f1' }
    : { background: '#ffffff', foreground: '#242423', cursor: '#8b6c00', cursorAccent: '#ffffff', selectionBackground: '#ffdf7080', black: '#242423', red: '#b03535', green: '#237345', yellow: '#806400', blue: '#305fa2', magenta: '#864788', cyan: '#256e78', white: '#777770', brightBlack: '#656560', brightRed: '#c43b3b', brightGreen: '#27814e', brightYellow: '#907000', brightBlue: '#376db9', brightMagenta: '#98519a', brightCyan: '#297d88', brightWhite: '#999990' };
}
