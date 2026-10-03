import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { TiltCover } from "./TiltCover";

describe("TiltCover activity", () => {
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("defers WebGL until visible and can draw again after hide/show", () => {
    let lost = false;
    const loseContext = vi.fn(() => {
      lost = true;
    });
    const gl = {
      createShader: vi.fn(() => (lost ? null : {})),
      shaderSource: vi.fn(),
      compileShader: vi.fn(),
      getShaderParameter: vi.fn(() => true),
      deleteShader: vi.fn(),
      createProgram: vi.fn(() => ({})),
      attachShader: vi.fn(),
      linkProgram: vi.fn(),
      getProgramParameter: vi.fn(() => true),
      useProgram: vi.fn(),
      createBuffer: vi.fn(() => ({})),
      bindBuffer: vi.fn(),
      bufferData: vi.fn(),
      getAttribLocation: vi.fn(() => 0),
      enableVertexAttribArray: vi.fn(),
      vertexAttribPointer: vi.fn(),
      getExtension: vi.fn((name: string) =>
        name === "WEBGL_lose_context" ? { loseContext } : null,
      ),
      createTexture: vi.fn(() => ({})),
      bindTexture: vi.fn(),
      texParameteri: vi.fn(),
      enable: vi.fn(),
      blendFunc: vi.fn(),
      getUniformLocation: vi.fn(() => ({})),
      pixelStorei: vi.fn(),
      texImage2D: vi.fn(),
      generateMipmap: vi.fn(),
      viewport: vi.fn(),
      clearColor: vi.fn(),
      clear: vi.fn(),
      uniform1f: vi.fn(),
      uniform2f: vi.fn(),
      drawArrays: vi.fn(),
      deleteTexture: vi.fn(),
      deleteBuffer: vi.fn(),
      deleteProgram: vi.fn(),
    };
    const getContext = vi
      .spyOn(HTMLCanvasElement.prototype, "getContext")
      .mockReturnValue(gl as unknown as WebGL2RenderingContext);
    const frames = new Map<number, FrameRequestCallback>();
    let frameId = 0;
    vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
      const id = ++frameId;
      frames.set(id, callback);
      return id;
    });
    vi.stubGlobal("cancelAnimationFrame", (id: number) => frames.delete(id));

    const { container, rerender } = render(
      <TiltCover active={false}>
        <img src="cover.jpg" alt="Cover" />
      </TiltCover>,
    );
    const outer = container.firstElementChild as HTMLElement;
    Object.defineProperty(outer, "clientWidth", { get: () => 320 });
    const img = container.querySelector("img")!;
    Object.defineProperties(img, {
      complete: { get: () => true },
      naturalWidth: { get: () => 640 },
    });
    outer.getBoundingClientRect = () =>
      ({
        left: 0,
        top: 0,
        width: 320,
        height: 320,
      }) as DOMRect;
    const hover = () => {
      fireEvent.mouseMove(outer, { clientX: 100, clientY: 100 });
      act(() => {
        const callbacks = [...frames.values()];
        frames.clear();
        callbacks.forEach((callback) => callback(16));
      });
    };

    expect(getContext).not.toHaveBeenCalled();
    hover();
    expect(frames.size).toBe(0);

    rerender(
      <TiltCover active>
        <img src="cover.jpg" alt="Cover" />
      </TiltCover>,
    );
    hover();
    expect(gl.drawArrays).toHaveBeenCalledTimes(1);
    expect(img.parentElement?.style.visibility).toBe("hidden");

    rerender(
      <TiltCover active={false}>
        <img src="cover.jpg" alt="Cover" />
      </TiltCover>,
    );
    expect(frames.size).toBe(0);
    expect(img.parentElement?.style.visibility).toBe("");
    expect(gl.deleteTexture).toHaveBeenCalledTimes(1);
    expect(gl.deleteBuffer).toHaveBeenCalledTimes(1);
    expect(gl.deleteProgram).toHaveBeenCalledTimes(1);

    rerender(
      <TiltCover active>
        <img src="cover.jpg" alt="Cover" />
      </TiltCover>,
    );
    hover();
    expect(loseContext).not.toHaveBeenCalled();
    expect(gl.drawArrays).toHaveBeenCalledTimes(2);
  });
});
