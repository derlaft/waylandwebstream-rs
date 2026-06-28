import { describe, expect, it } from 'vitest';
import { attachSoftKeyboard } from './softKeyboard';
import type { ClientMessage } from './protocol';

// ─── Helpers ─────────────────────────────────────────────────────────────────

function setup(): { input: HTMLTextAreaElement; messages: ClientMessage[]; detach: () => void } {
  const input = document.createElement('textarea');
  document.body.appendChild(input);
  const messages: ClientMessage[] = [];
  const detach = attachSoftKeyboard(input, (m) => messages.push(m));
  return { input, messages, detach };
}

// Simulates the browser mutating the field then firing `input` -- the same
// shape the diff-based handler reconciles against.
function type(input: HTMLTextAreaElement, value: string): void {
  input.value = value;
  input.dispatchEvent(new InputEvent('input', { inputType: 'insertText' }));
}

function fireBeforeInput(input: HTMLTextAreaElement, inputType: string): void {
  input.dispatchEvent(new InputEvent('beforeinput', { inputType, cancelable: true }));
}

// keydown/keyup are sent as a pair; this builds the expected message pair.
function key(code: string): ClientMessage[] {
  return [
    { type: 'key', eventType: 'keydown', code },
    { type: 'key', eventType: 'keyup', code },
  ];
}

function shiftedKey(code: string): ClientMessage[] {
  return [
    { type: 'key', eventType: 'keydown', code: 'ShiftLeft' },
    { type: 'key', eventType: 'keydown', code },
    { type: 'key', eventType: 'keyup', code },
    { type: 'key', eventType: 'keyup', code: 'ShiftLeft' },
  ];
}

// ─── Field value mirroring (diff) ──────────────────────────────────────────────

describe('attachSoftKeyboard value mirroring', () => {
  it('maps a lowercase letter to a bare key tap', () => {
    const { input, messages } = setup();
    type(input, 'a');
    expect(messages).toEqual(key('KeyA'));
  });

  it('wraps an uppercase letter in Shift', () => {
    const { input, messages } = setup();
    type(input, 'A');
    expect(messages).toEqual(shiftedKey('KeyA'));
  });

  it('maps a shifted digit symbol to its digit key with Shift', () => {
    const { input, messages } = setup();
    type(input, '!');
    expect(messages).toEqual(shiftedKey('Digit1'));
  });

  it('emits only the added tail as text grows', () => {
    const { input, messages } = setup();
    type(input, 'a');
    type(input, 'ab');
    type(input, 'abc');
    expect(messages).toEqual([...key('KeyA'), ...key('KeyB'), ...key('KeyC')]);
  });

  it('emits a Backspace when the field shrinks', () => {
    const { input, messages } = setup();
    type(input, 'ab');
    messages.length = 0;
    type(input, 'a');
    expect(messages).toEqual(key('Backspace'));
  });

  it('mirrors an autocorrect rewrite as Backspaces + the new tail', () => {
    const { input, messages } = setup();
    type(input, 'teh');
    messages.length = 0;
    // Autocorrect replaces the whole word in one shot: "teh" -> "the".
    type(input, 'the');
    expect(messages).toEqual([...key('Backspace'), ...key('Backspace'), ...key('KeyH'), ...key('KeyE')]);
  });

  it('drops characters with no US-layout key (e.g. emoji)', () => {
    const { input, messages } = setup();
    type(input, '😀');
    expect(messages).toEqual([]);
  });
});

// ─── Composition (predictive text / glide typing) ──────────────────────────────

describe('attachSoftKeyboard composition', () => {
  it('emits nothing for intermediate composition, then the word once at end', () => {
    const { input, messages } = setup();
    input.dispatchEvent(new Event('compositionstart'));
    // Cumulative composition snapshots -- these must NOT each be transcribed.
    type(input, 'h');
    type(input, 'he');
    type(input, 'hel');
    type(input, 'hello');
    expect(messages).toEqual([]);
    input.dispatchEvent(new Event('compositionend'));
    expect(messages).toEqual([...key('KeyH'), ...key('KeyE'), ...key('KeyL'), ...key('KeyL'), ...key('KeyO')]);
  });
});

// ─── beforeinput-only keys ─────────────────────────────────────────────────────

describe('attachSoftKeyboard beforeinput keys', () => {
  it('maps line break / paragraph to Enter and clears the buffer', () => {
    const { input, messages } = setup();
    fireBeforeInput(input, 'insertParagraph');
    expect(messages).toEqual(key('Enter'));
    expect(input.value).toBe('');
  });

  it('sends a Backspace when deleting on an empty buffer', () => {
    const { input, messages } = setup();
    fireBeforeInput(input, 'deleteContentBackward');
    expect(messages).toEqual(key('Backspace'));
  });

  it('does not double-send Backspace when the buffer has text (diff handles it)', () => {
    const { input, messages } = setup();
    type(input, 'ab');
    messages.length = 0;
    // Real backspace: beforeinput fires, then the field shrinks + input fires.
    fireBeforeInput(input, 'deleteContentBackward');
    type(input, 'a');
    expect(messages).toEqual(key('Backspace'));
  });
});

// ─── keydown/keyup passthrough ─────────────────────────────────────────────────

describe('attachSoftKeyboard physical-key passthrough', () => {
  it('forwards navigation keys by physical code', () => {
    const { input, messages } = setup();
    input.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowUp', code: 'ArrowUp' }));
    input.dispatchEvent(new KeyboardEvent('keyup', { key: 'ArrowUp', code: 'ArrowUp' }));
    expect(messages).toEqual([
      { type: 'key', eventType: 'keydown', code: 'ArrowUp' },
      { type: 'key', eventType: 'keyup', code: 'ArrowUp' },
    ]);
  });

  it('ignores plain printable keydowns (value mirroring owns them)', () => {
    const { input, messages } = setup();
    input.dispatchEvent(new KeyboardEvent('keydown', { key: 'a', code: 'KeyA' }));
    expect(messages).toEqual([]);
  });

  it('ignores Enter/Backspace keydowns (beforeinput owns them)', () => {
    const { input, messages } = setup();
    input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', code: 'Enter' }));
    input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Backspace', code: 'Backspace' }));
    expect(messages).toEqual([]);
  });

  it('forwards modified combos by physical code', () => {
    const { input, messages } = setup();
    input.dispatchEvent(new KeyboardEvent('keydown', { key: 'c', code: 'KeyC', ctrlKey: true }));
    expect(messages).toEqual([{ type: 'key', eventType: 'keydown', code: 'KeyC' }]);
  });
});
