import { describe, it, expect, vi } from 'vitest';

import { WebGlVideoRenderer } from './glRenderer';

// A minimal WebGL stand-in: every property is a stable spy that returns a
// truthy token, so create*/getUniformLocation are non-null and
// get*Parameter (LINK_STATUS/COMPILE_STATUS) read as success, while the GL
// constants used as call arguments are just distinct truthy values. The point
// is to observe which upload call `draw` makes, not to render anything.
function makeMockGl() {
  const cache = new Map<string, ReturnType<typeof vi.fn>>();
  const token = { mock: true };
  return new Proxy(
    {},
    {
      get(_target, prop: string) {
        let fn = cache.get(prop);
        if (!fn) {
          fn = vi.fn(() => token);
          cache.set(prop, fn);
        }
        return fn;
      },
    },
  ) as unknown as WebGLRenderingContext & Record<string, ReturnType<typeof vi.fn>>;
}

function makeFrame(codedWidth: number, codedHeight: number): VideoFrame {
  return { codedWidth, codedHeight } as unknown as VideoFrame;
}

describe('WebGlVideoRenderer upload path', () => {
  it('allocates once per size with texImage2D, then updates in place with texSubImage2D', () => {
    const gl = makeMockGl();
    const canvas = { width: 1280, height: 720, addEventListener: vi.fn() } as unknown as HTMLCanvasElement;
    const renderer = new WebGlVideoRenderer(canvas, gl, 'webgl2');

    const frame = makeFrame(1280, 720);

    // First frame of this size: allocate storage.
    renderer.draw(frame);
    expect(gl.texImage2D).toHaveBeenCalledTimes(1);
    expect(gl.texSubImage2D).toHaveBeenCalledTimes(0);

    // Same size again: update in place, no reallocation.
    renderer.draw(frame);
    renderer.draw(frame);
    expect(gl.texImage2D).toHaveBeenCalledTimes(1);
    expect(gl.texSubImage2D).toHaveBeenCalledTimes(2);
  });

  it('reallocates with texImage2D when the frame size changes', () => {
    const gl = makeMockGl();
    const canvas = { width: 800, height: 592, addEventListener: vi.fn() } as unknown as HTMLCanvasElement;
    const renderer = new WebGlVideoRenderer(canvas, gl, 'webgl2');

    renderer.draw(makeFrame(800, 592)); // alloc A
    renderer.draw(makeFrame(800, 592)); // in place
    renderer.draw(makeFrame(640, 480)); // size change -> realloc
    renderer.draw(makeFrame(640, 480)); // in place at new size

    expect(gl.texImage2D).toHaveBeenCalledTimes(2);
    expect(gl.texSubImage2D).toHaveBeenCalledTimes(2);
  });
});
