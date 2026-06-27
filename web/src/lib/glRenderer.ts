// Video frame -> canvas blit, abstracted over a backend.
//
// The 2D path (`ctx.drawImage(VideoFrame)`) forces a synchronous
// GPU->CPU->GPU readback on some browsers -- most painfully Firefox, where
// it runs on the main thread and stalls the VideoDecoder's `output` callback
// long enough for the decode queue to back up. That backup used to cascade
// into a `request_keyframe` and (until the server-side fix) a shared-encoder
// bitrate cut. Uploading the frame straight into a GL texture and drawing a
// fullscreen quad keeps the pixels on the GPU the whole way, eliminating the
// readback.
//
// `createVideoRenderer` prefers WebGL and falls back to the 2D context when
// WebGL is unavailable. A canvas can only ever hand out one context type, so
// the fallback is only safe while the canvas is still untainted: we only
// reach for 2D when `getContext('webgl')` returns null (no context was
// created), never after a WebGL context exists.

// Both the on-screen canvas (main-thread fallback) and the worker's
// OffscreenCanvas (Stage B) are valid render targets; everything the renderer
// touches -- getContext, width/height, the context-loss events -- exists on
// both.
export type RenderCanvas = HTMLCanvasElement | OffscreenCanvas;

export type RendererBackend = 'webgl' | 'webgl2' | '2d';

export interface VideoRenderer {
  /// Upload and draw one decoded frame to the canvas. The canvas backing
  /// size is owned by the caller (see `ensureCanvasSize` in stream.ts); the
  /// renderer reads `canvas.width/height` and adjusts its viewport to match.
  draw(frame: VideoFrame): void;
  /// Which backend is live. Surfaced for diagnostics (e.g. confirming the
  /// fast path is actually in use on a given browser).
  readonly backend: RendererBackend;
}

const VERTEX_SHADER = `
attribute vec2 a_pos;
attribute vec2 a_uv;
varying vec2 v_uv;
void main() {
  v_uv = a_uv;
  gl_Position = vec4(a_pos, 0.0, 1.0);
}`;

const FRAGMENT_SHADER = `
precision mediump float;
uniform sampler2D u_tex;
varying vec2 v_uv;
void main() {
  gl_FragColor = texture2D(u_tex, v_uv);
}`;

// A fullscreen quad as a triangle strip: interleaved clip-space position
// (x, y) and texture coordinate (u, v). The v coordinate is flipped relative
// to position because texImage2D uploads the frame's first (top) row to v=0,
// while clip-space +y is the top of the screen -- without the flip the image
// renders upside down.
const QUAD = new Float32Array([
  -1, -1, 0, 1,
  1, -1, 1, 1,
  -1, 1, 0, 0,
  1, 1, 1, 0,
]);

class WebGlVideoRenderer implements VideoRenderer {
  readonly backend: 'webgl' | 'webgl2';

  private readonly canvas: RenderCanvas;
  private readonly gl: WebGLRenderingContext;
  private program: WebGLProgram | null = null;
  private buffer: WebGLBuffer | null = null;
  private texture: WebGLTexture | null = null;
  // WebGL contexts can be lost at any time -- common on mobile when the tab
  // is backgrounded or the GPU is reclaimed. While lost, draws are no-ops;
  // resources are rebuilt on restore. (The caller still closes the frame, so
  // a no-op draw doesn't leak.)
  private contextLost = false;
  private lastW = -1;
  private lastH = -1;

  constructor(canvas: RenderCanvas, gl: WebGLRenderingContext, backend: 'webgl' | 'webgl2') {
    this.canvas = canvas;
    this.gl = gl;
    this.backend = backend;
    canvas.addEventListener('webglcontextlost', this.onContextLost);
    canvas.addEventListener('webglcontextrestored', this.onContextRestored);
    this.initResources();
  }

  private onContextLost = (e: Event): void => {
    // Default behavior makes the loss permanent; preventing it lets the
    // browser fire `webglcontextrestored` so we can rebuild.
    e.preventDefault();
    this.contextLost = true;
    console.warn('WebGL context lost; pausing video render until restored');
  };

  private onContextRestored = (): void => {
    console.info('WebGL context restored; rebuilding video render resources');
    this.lastW = -1;
    this.lastH = -1;
    this.initResources();
    this.contextLost = false;
  };

  private initResources(): void {
    const gl = this.gl;
    const program = this.linkProgram(VERTEX_SHADER, FRAGMENT_SHADER);

    this.buffer = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, this.buffer);
    gl.bufferData(gl.ARRAY_BUFFER, QUAD, gl.STATIC_DRAW);

    const stride = 4 * Float32Array.BYTES_PER_ELEMENT;
    const posLoc = gl.getAttribLocation(program, 'a_pos');
    gl.enableVertexAttribArray(posLoc);
    gl.vertexAttribPointer(posLoc, 2, gl.FLOAT, false, stride, 0);
    const uvLoc = gl.getAttribLocation(program, 'a_uv');
    gl.enableVertexAttribArray(uvLoc);
    gl.vertexAttribPointer(uvLoc, 2, gl.FLOAT, false, stride, 2 * Float32Array.BYTES_PER_ELEMENT);

    this.texture = gl.createTexture();
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.texture);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);

    gl.useProgram(program);
    gl.uniform1i(gl.getUniformLocation(program, 'u_tex'), 0);
    this.program = program;
  }

  private linkProgram(vsSource: string, fsSource: string): WebGLProgram {
    const gl = this.gl;
    const vs = this.compileShader(gl.VERTEX_SHADER, vsSource);
    const fs = this.compileShader(gl.FRAGMENT_SHADER, fsSource);
    const program = gl.createProgram();
    if (!program) throw new Error('gl.createProgram returned null');
    gl.attachShader(program, vs);
    gl.attachShader(program, fs);
    gl.linkProgram(program);
    // Shaders are deletable once linked into the program.
    gl.deleteShader(vs);
    gl.deleteShader(fs);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
      const log = gl.getProgramInfoLog(program);
      throw new Error(`WebGL program link failed: ${log}`);
    }
    return program;
  }

  private compileShader(type: number, source: string): WebGLShader {
    const gl = this.gl;
    const shader = gl.createShader(type);
    if (!shader) throw new Error('gl.createShader returned null');
    gl.shaderSource(shader, source);
    gl.compileShader(shader);
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
      const log = gl.getShaderInfoLog(shader);
      gl.deleteShader(shader);
      throw new Error(`WebGL shader compile failed: ${log}`);
    }
    return shader;
  }

  draw(frame: VideoFrame): void {
    if (this.contextLost) return;
    const gl = this.gl;
    const w = this.canvas.width;
    const h = this.canvas.height;
    if (w !== this.lastW || h !== this.lastH) {
      gl.viewport(0, 0, w, h);
      this.lastW = w;
      this.lastH = h;
    }
    // Texture, program, buffer and attribute pointers are all left bound from
    // initResources -- nothing else touches GL state -- so per frame is just
    // the upload plus the draw.
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, frame);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
  }
}

class Canvas2dVideoRenderer implements VideoRenderer {
  readonly backend = '2d' as const;
  private readonly ctx: CanvasRenderingContext2D | OffscreenCanvasRenderingContext2D;

  constructor(canvas: RenderCanvas) {
    // getContext's overloads don't resolve cleanly over the canvas union;
    // narrow to one member for the call (both return a drawImage-capable 2D
    // context at runtime).
    const ctx = (canvas as HTMLCanvasElement).getContext('2d');
    if (!ctx) throw new Error('2D canvas context unavailable');
    this.ctx = ctx;
  }

  draw(frame: VideoFrame): void {
    this.ctx.drawImage(frame, 0, 0);
  }
}

// Acquire a WebGL context for the canvas, or null if WebGL isn't available.
//
// Only the plain opaque/no-buffer attributes are requested. We deliberately do
// NOT ask for `desynchronized` (a low-latency presentation hint) or a
// `powerPreference`: those are meant to be ignored when unsupported, but in
// practice some Android Chrome builds return *null* for `desynchronized` -- on
// the worker OffscreenCanvas and on the on-screen canvas alike -- which would
// drop a perfectly WebGL-capable device to the slow 2D readback path. The
// marginal latency win isn't worth losing WebGL over.
//
// `'experimental-webgl'` is not tried: it's valid on a regular canvas but
// *throws* on an OffscreenCanvas (not in its context-id enum), and no
// WebCodecs-capable browser needs it. Each getContext is still wrapped
// defensively so nothing can throw out of acquisition and crash the worker.
function acquireWebGl(
  canvas: RenderCanvas,
): { gl: WebGLRenderingContext; id: 'webgl' | 'webgl2' } | null {
  const attrs: WebGLContextAttributes = {
    alpha: false,
    depth: false,
    stencil: false,
    antialias: false,
  };
  // getContext's overloads don't resolve over the canvas union; narrow to one
  // member for the call -- both expose the same context ids at runtime.
  const target = canvas as HTMLCanvasElement;
  for (const id of ['webgl', 'webgl2'] as const) {
    let gl: WebGLRenderingContext | null = null;
    try {
      gl = target.getContext(id, attrs) as WebGLRenderingContext | null;
    } catch {
      gl = null;
    }
    if (gl) return { gl, id };
  }
  return null;
}

export function createVideoRenderer(canvas: RenderCanvas): VideoRenderer {
  const acquired = acquireWebGl(canvas);
  if (acquired) {
    try {
      return new WebGlVideoRenderer(canvas, acquired.gl, acquired.id);
    } catch (e) {
      // The canvas is now WebGL-tainted, so we can't get a 2D context from
      // it. This only happens if a live context fails to compile a trivial
      // shader, which is effectively never -- surface it rather than hide a
      // broken pipeline behind a fallback that can't work anyway.
      console.error('WebGL renderer init failed on a live context:', e);
      throw e;
    }
  }

  // No WebGL: the canvas was never tainted, so 2D is still available. (On the
  // worker path this is pre-empted by the WebGL probe in videoClient.ts, which
  // keeps a worker that can't render off the canvas; this fallback is mainly
  // the main-thread path on a WebGL-less browser.) The active backend is
  // surfaced in the stats panel rather than logged.
  return new Canvas2dVideoRenderer(canvas);
}
