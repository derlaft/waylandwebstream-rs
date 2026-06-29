import { describe, it, expect } from 'vitest';

import { Viewport } from './viewport';

describe('Viewport canvas CSS sizing', () => {
  it('writes the canvas CSS size only once for repeated updates at the same size', () => {
    // start() runs update() more than once at the same viewport size (the
    // nativeResolution store emits synchronously on subscribe, then the
    // explicit update() at the end). Writing canvas.style invalidates layout,
    // so the idempotent write must be skipped when the size is unchanged --
    // otherwise a momentum-scroll flood of visualViewport events thrashes
    // layout every frame.
    let widthWrites = 0;
    let heightWrites = 0;
    const style = {} as CSSStyleDeclaration;
    Object.defineProperty(style, 'width', {
      set: () => {
        widthWrites++;
      },
      get: () => '',
      configurable: true,
    });
    Object.defineProperty(style, 'height', {
      set: () => {
        heightWrites++;
      },
      get: () => '',
      configurable: true,
    });
    const canvas = { style, width: 0, height: 0 } as unknown as HTMLCanvasElement;

    const vp = new Viewport({ canvas, sendControl: () => {} });
    vp.start();
    vp.stop();

    expect(widthWrites).toBe(1);
    expect(heightWrites).toBe(1);
  });
});
