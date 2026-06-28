// On-screen keyboard support for touch devices. The stream is a bare
// <canvas>, so a mobile browser never shows its native soft keyboard. The
// OnScreenKeyboard component focuses a hidden field to summon that keyboard;
// this module translates what gets typed into the existing `wl_keyboard`
// keycode pipeline so it works in *every* remote app (incl. XWayland),
// rather than the text-input/input-method route which only reaches
// text-input-aware Wayland clients.
//
// Mobile soft keyboards don't emit reliable `KeyboardEvent.code` values
// (Android/GBoard reports keyCode 229 / "Unidentified" for character keys),
// so character entry is captured from `beforeinput` events and translated
// to US-layout key events here. The server's own XKB keymap (us) then
// resolves the keysym from the physical key, exactly as for hardware keys
// (see src/input/keyboard.rs). Characters with no US-layout key (emoji,
// non-Latin) can't be expressed this way and are dropped -- a future
// zwp_virtual_keyboard_v1 path could carry full Unicode.
import type { ClientMessage } from './protocol';
import { persisted } from './persisted';

// ─── Persisted settings ──────────────────────────────────────────────────────

/// Whether the floating on-screen-keyboard button is shown. Off by default;
/// toggled from the side panel (KeyboardToggleButton).
export const onScreenKeyboardEnabled = persisted('osk.enabled', false);

/// Last position of the floating button, in CSS px from the stage's
/// top-left. `null` until first placed -- the component then defaults it to
/// the bottom-right corner based on the current viewport size.
export const fabPosition = persisted<{ x: number; y: number } | null>('osk.fabPos', null);

// ─── US-layout character → physical key ───────────────────────────────────────

export interface KeyStroke {
  /// `KeyboardEvent.code` of the physical key (what the wire protocol sends).
  code: string;
  /// Whether the US layout needs Shift held to produce this character.
  shift: boolean;
}

/// Printable ASCII → the physical US key (and whether Shift is needed) that
/// produces it. Built from letters/digits plus the explicit symbol rows so
/// the server's us XKB keymap resolves the right keysym.
export const US_CHAR_TO_KEY: Record<string, KeyStroke> = buildUsCharMap();

function buildUsCharMap(): Record<string, KeyStroke> {
  const map: Record<string, KeyStroke> = {};

  // Letters: lowercase unshifted, uppercase shifted.
  for (let i = 0; i < 26; i++) {
    const lower = String.fromCharCode(97 + i); // a..z
    const upper = String.fromCharCode(65 + i); // A..Z
    const code = `Key${upper}`;
    map[lower] = { code, shift: false };
    map[upper] = { code, shift: true };
  }

  // Digit row: unshifted digit, shifted symbol.
  const digitRow: Array<[string, string, string]> = [
    ['1', '!', 'Digit1'],
    ['2', '@', 'Digit2'],
    ['3', '#', 'Digit3'],
    ['4', '$', 'Digit4'],
    ['5', '%', 'Digit5'],
    ['6', '^', 'Digit6'],
    ['7', '&', 'Digit7'],
    ['8', '*', 'Digit8'],
    ['9', '(', 'Digit9'],
    ['0', ')', 'Digit0'],
  ];
  for (const [digit, sym, code] of digitRow) {
    map[digit] = { code, shift: false };
    map[sym] = { code, shift: true };
  }

  // Punctuation keys: unshifted char, shifted char.
  const punct: Array<[string, string, string]> = [
    ['`', '~', 'Backquote'],
    ['-', '_', 'Minus'],
    ['=', '+', 'Equal'],
    ['[', '{', 'BracketLeft'],
    [']', '}', 'BracketRight'],
    ['\\', '|', 'Backslash'],
    [';', ':', 'Semicolon'],
    ["'", '"', 'Quote'],
    [',', '<', 'Comma'],
    ['.', '>', 'Period'],
    ['/', '?', 'Slash'],
  ];
  for (const [plain, shifted, code] of punct) {
    map[plain] = { code, shift: false };
    map[shifted] = { code, shift: true };
  }

  map[' '] = { code: 'Space', shift: false };
  map['\t'] = { code: 'Tab', shift: false };

  return map;
}

// ─── Translation ──────────────────────────────────────────────────────────────

type Send = (msg: ClientMessage) => void;

/// Emits one character as a Shift-wrapped key tap. Shift is pressed/released
/// around the key so it composes correctly with the server's XKB modifier
/// tracking and never lingers.
function sendChar(send: Send, stroke: KeyStroke): void {
  if (stroke.shift) send({ type: 'key', eventType: 'keydown', code: 'ShiftLeft' });
  send({ type: 'key', eventType: 'keydown', code: stroke.code });
  send({ type: 'key', eventType: 'keyup', code: stroke.code });
  if (stroke.shift) send({ type: 'key', eventType: 'keyup', code: 'ShiftLeft' });
}

/// Emits a bare key tap (no Shift), for keys whose `code` is known directly.
function sendKey(send: Send, code: string): void {
  send({ type: 'key', eventType: 'keydown', code });
  send({ type: 'key', eventType: 'keyup', code });
}

/// Translates a run of typed text into key taps, dropping characters that
/// have no US-layout key.
export function typeText(send: Send, text: string): void {
  for (const ch of text) {
    const stroke = US_CHAR_TO_KEY[ch];
    if (stroke) sendChar(send, stroke);
    else console.debug(`softKeyboard: no US-layout key for ${JSON.stringify(ch)}`);
  }
}

// Enter/Backspace/Delete arrive as `beforeinput` edit intents on both soft
// and hardware keyboards (in a <textarea>), so they're handled there and
// excluded from the keydown passthrough below to avoid double entry.
const BEFOREINPUT_KEYS = new Set(['Enter', 'Backspace', 'Delete']);

/// True when an event is something `beforeinput` already covers: a plain
/// printable character, or Enter/Backspace/Delete -- but never a modified
/// combo (Ctrl+C etc.), which must go through the physical-code path.
function handledByBeforeInput(e: KeyboardEvent): boolean {
  if (e.ctrlKey || e.metaKey || e.altKey) return false;
  if (e.key.length === 1) return true;
  return BEFOREINPUT_KEYS.has(e.key);
}

// Once the hidden field has accumulated this many characters, trim it back
// to the last `BUFFER_KEEP` so it never grows without bound during a long
// session. Trimming the *front* keeps the diff aligned (see `flush`).
const BUFFER_TRIM_AT = 64;
const BUFFER_KEEP = 16;

/// Wires the hidden field that drives the device's native soft keyboard.
///
/// Rather than transcribing raw `beforeinput` events -- which on mobile
/// arrive as cumulative composition snapshots (predictive text, autocorrect,
/// glide typing) and would duplicate or mangle text -- this lets the field
/// accumulate normally and **mirrors its value as a diff**: on each settled
/// change it emits Backspaces for the removed tail and key taps for the added
/// tail. So the remote app sees exactly what the field shows, including
/// autocorrect rewrites (e.g. "teh"->"the" becomes Backspace×2 + "he").
///
/// `beforeinput` is used only for Enter and for deletes that can't shrink the
/// field (it's already empty, so no `input` would fire). Navigation/function
/// keys and modified combos fall back to the physical `code` path so a paired
/// Bluetooth keyboard still gets full coverage. Returns a detach function.
export function attachSoftKeyboard(input: HTMLTextAreaElement, send: Send): () => void {
  // Last value we reconciled against, and whether an IME composition is in
  // progress (we wait for it to finish before diffing, so intermediate
  // composition states don't each produce keystrokes).
  let prev = '';
  let composing = false;

  // Reconciles the field's current value against `prev`, emitting the
  // minimal Backspace + key-tap sequence, then bounds the buffer size.
  const flush = (): void => {
    const cur = input.value;
    let common = 0;
    const max = Math.min(prev.length, cur.length);
    while (common < max && prev[common] === cur[common]) common++;
    for (let i = 0; i < prev.length - common; i++) sendKey(send, 'Backspace');
    typeText(send, cur.slice(common));
    prev = cur;
    if (cur.length > BUFFER_TRIM_AT) {
      // Trim the front; `prev` stays equal to the field so future diffs line
      // up. Setting .value programmatically doesn't fire an `input` event.
      const keep = cur.slice(-BUFFER_KEEP);
      input.value = keep;
      prev = keep;
    }
  };

  const reset = (): void => {
    input.value = '';
    prev = '';
  };

  const onBeforeInput = (e: Event): void => {
    const ie = e as InputEvent;
    switch (ie.inputType) {
      case 'insertLineBreak':
      case 'insertParagraph':
        // Don't let a newline land in the buffer; send a real Enter instead.
        e.preventDefault();
        sendKey(send, 'Enter');
        reset();
        break;
      case 'deleteContentBackward':
        // When the buffer already holds text, let the field shrink and the
        // diff in `onInput` emit the Backspace. When it's empty there's
        // nothing to shrink (no `input` fires), so emit one here.
        if (!composing && prev.length === 0) {
          e.preventDefault();
          sendKey(send, 'Backspace');
        }
        break;
      case 'deleteContentForward':
        if (!composing && prev.length === 0) {
          e.preventDefault();
          sendKey(send, 'Delete');
        }
        break;
      default:
        break;
    }
  };

  const onInput = (): void => {
    if (composing) return; // wait for compositionend to diff the final text
    flush();
  };

  const onCompositionStart = (): void => {
    composing = true;
  };
  const onCompositionEnd = (): void => {
    composing = false;
    flush();
  };

  // Start each focus session from a clean slate so a stale buffer can't make
  // the first diff emit spurious Backspaces.
  const onFocus = reset;
  const onBlur = reset;

  const onKeyDown = (e: KeyboardEvent): void => {
    if (handledByBeforeInput(e) || !e.code) return;
    e.preventDefault();
    send({ type: 'key', eventType: 'keydown', code: e.code });
  };
  const onKeyUp = (e: KeyboardEvent): void => {
    if (handledByBeforeInput(e) || !e.code) return;
    e.preventDefault();
    send({ type: 'key', eventType: 'keyup', code: e.code });
  };

  input.addEventListener('beforeinput', onBeforeInput);
  input.addEventListener('input', onInput);
  input.addEventListener('compositionstart', onCompositionStart);
  input.addEventListener('compositionend', onCompositionEnd);
  input.addEventListener('focus', onFocus);
  input.addEventListener('blur', onBlur);
  input.addEventListener('keydown', onKeyDown);
  input.addEventListener('keyup', onKeyUp);

  return () => {
    input.removeEventListener('beforeinput', onBeforeInput);
    input.removeEventListener('input', onInput);
    input.removeEventListener('compositionstart', onCompositionStart);
    input.removeEventListener('compositionend', onCompositionEnd);
    input.removeEventListener('focus', onFocus);
    input.removeEventListener('blur', onBlur);
    input.removeEventListener('keydown', onKeyDown);
    input.removeEventListener('keyup', onKeyUp);
  };
}
