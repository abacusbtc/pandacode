// A server-side folder picker for remote mode.
//
// The native dialog returns a path on *this* computer, which does not exist on the
// gateway — the defect D6 exists for. Directories only: `gateway_list_dirs` (Task 7)
// is the only listing M1 serves, and a project has to be a folder. Navigation always
// follows a listing the server sent; the client never invents a path it was not shown.

import { call } from './bridge';
import { escape } from './dom';

/** One entry as `gateway_list_dirs` returns it. Non-folders are filtered server-side too. */
type Entry = { name: string; isDir: boolean };
/** `path` is the server's absolute folder; `parent` is null at the root of the filesystem. */
type Listing = { path: string; parent?: string | null; entries?: Entry[] };

export type RemoteFilesShell = {
  /** Opens the app's dialog shell with this body and returns the dialog element. */
  open(title: string, body: string): HTMLDialogElement;
  close(): void;
};

/**
 * The server's separator, taken from a path it sent. Unix listings contain `/`; a
 * Windows gateway sends `C:\…` with backslashes and no forward slash.
 */
function joinPath(base: string, name: string): string {
  if (base.endsWith('/') || base.endsWith('\\')) return base + name;
  return `${base}${base.includes('\\') && !base.includes('/') ? '\\' : '/'}${name}`;
}

/**
 * Asks the gateway for its folders and resolves the chosen server path, or null when
 * the person cancels. `Enter` opens the highlighted folder, `Backspace` goes up,
 * `Escape` closes; every step is a real request, so a slow or failing server shows
 * what happened instead of a folder list that looks fine.
 */
export function pickRemoteFolder(shell: RemoteFilesShell, title = 'Open a project on the server'): Promise<string | null> {
  const dialog = shell.open(title, `<div class="remote-files">
    <div class="remote-files-bar"><button type="button" class="secondary-button" data-remote-action="up" disabled>Up</button><span class="remote-files-path" data-remote-path role="status">Server home</span></div>
    <div class="remote-files-list" data-remote-list role="group" aria-label="Server folders" tabindex="-1"><p class="remote-files-empty" role="status">Loading…</p></div>
    <div class="dialog-footer"><button type="button" class="secondary-button" data-action="close-dialog">Cancel</button><button type="button" class="primary-button use-folder" data-remote-action="use" disabled>Use this folder</button></div>
  </div>`);

  return new Promise<string | null>(resolve => {
    const list = dialog.querySelector<HTMLElement>('[data-remote-list]')!;
    const pathLabel = dialog.querySelector<HTMLElement>('[data-remote-path]')!;
    const up = dialog.querySelector<HTMLButtonElement>('[data-remote-action="up"]')!;
    const use = dialog.querySelector<HTMLButtonElement>('[data-remote-action="use"]')!;
    let current: Listing | null = null;
    // The last folder the server listed, kept across a failure so Up/Backspace can return
    // to it: a failed navigation leaves no parent to read.
    let here: string | null = null;
    let failure = '';
    let loading = true;
    let index = 0;
    let request = 0;
    let done = false;

    const folders = (): Entry[] => (current?.entries ?? []).filter(entry => entry.isDir);
    /** Up leaves a listed folder, and leaves a failure for the folder that worked. */
    const upTarget = (): string | null => (current ? current.parent ?? null : here);

    // The Open project dialog's Browse swaps this content in *during* the click gesture
    // that opened it, so the trailing click of that same gesture arrives with the pointer
    // already over a row — and would navigate, or on the footer choose a folder nobody
    // looked at. Clicks are ignored for a moment after opening; a new gesture is always
    // far slower than that.
    const openedAt = performance.now();

    function finish(path: string | null): void {
      if (done) return;
      done = true;
      dialog.removeEventListener('click', onClick);
      document.removeEventListener('keydown', onKey);
      dialog.removeEventListener('close', onClose);
      shell.close();
      resolve(path);
    }

    /** The dialog's own close — the ✕ button, Cancel, or Escape — is a cancel. */
    function onClose(): void { finish(null); }

    function render(): void {
      pathLabel.textContent = current?.path || here || 'Server home';
      up.disabled = loading || !upTarget();
      use.disabled = loading || !current;
      if (loading) { list.innerHTML = '<p class="remote-files-empty" role="status">Loading…</p>'; return; }
      if (failure) { list.innerHTML = `<p class="remote-files-error" role="alert">${escape(failure)}</p>`; return; }
      const entries = folders();
      if (!entries.length) { list.innerHTML = '<p class="remote-files-empty" role="status">No folders here</p>'; return; }
      list.innerHTML = entries.map((entry, position) => `<button type="button" class="entry" data-remote-name="${escape(entry.name)}" tabindex="${position === index ? 0 : -1}">${escape(entry.name)}</button>`).join('');
    }

    /** `path` omitted asks for the server's home; every other path was sent by the server. */
    async function load(path?: string | null): Promise<void> {
      const ticket = ++request;
      loading = true; failure = ''; index = 0;
      render();
      try {
        const listing = await call<Listing>('gateway_list_dirs', path === undefined ? {} : { path });
        if (ticket !== request || done) return;
        current = listing;
        here = listing.path;
      } catch (error) {
        if (ticket !== request || done) return;
        // The server's own string ("not_a_directory: …", or a path and its io error) —
        // never a stack trace.
        current = null;
        failure = typeof error === 'string' ? error : error instanceof Error ? error.message : String(error);
      } finally {
        if (ticket !== request || done) return;
        loading = false;
        render();
        // Keep the keyboard inside the picker: a re-render removes the row that had focus.
        if (!dialog.contains(document.activeElement)) {
          (list.querySelector<HTMLElement>('.entry') ?? list).focus();
        }
      }
    }

    function onClick(event: MouseEvent): void {
      if (performance.now() - openedAt < 250) return;
      const target = event.target instanceof Element ? event.target : null;
      if (!target) return;
      const action = target.closest<HTMLElement>('[data-remote-action]')?.dataset.remoteAction;
      if (action === 'up') { const target = upTarget(); if (target) void load(target); return; }
      if (action === 'use') { if (current && !loading) finish(current.path); return; }
      const entry = target.closest<HTMLElement>('[data-remote-name]');
      if (entry && current) void load(joinPath(current.path, entry.dataset.remoteName ?? ''));
    }

    function onKey(event: KeyboardEvent): void {
      // Escape is handled here rather than left to the dialog: re-rendering the list can
      // drop focus to the body, where a modal dialog no longer reacts to it.
      if (event.key === 'Escape') { event.preventDefault(); finish(null); return; }
      if (event.key === 'Backspace') {
        const target = upTarget();
        if (target && !loading) { event.preventDefault(); void load(target); }
        return;
      }
      const entries = folders();
      if (!entries.length || loading) return;
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        event.preventDefault();
        index = (index + (event.key === 'ArrowDown' ? 1 : entries.length - 1)) % entries.length;
        render();
        list.querySelectorAll<HTMLElement>('.entry')[index]?.focus();
        return;
      }
      // Enter opens the highlighted folder. On a button (Up, Use this folder) the key
      // keeps its own meaning, so only an entry is handled here.
      if (event.key === 'Enter') {
        const focused = document.activeElement;
        if (!(focused instanceof HTMLElement) || !focused.matches('[data-remote-name]')) return;
        event.preventDefault();
        if (current) void load(joinPath(current.path, focused.dataset.remoteName ?? ''));
      }
    }

    dialog.addEventListener('click', onClick);
    // On the document, not the dialog: re-rendering the list removes the focused row, and
    // focus then falls back to the body — where a dialog-scoped listener never fires.
    document.addEventListener('keydown', onKey);
    dialog.addEventListener('close', onClose);
    void load();
  });
}
