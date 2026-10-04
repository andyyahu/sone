import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createImageAdmission } from "./imageAdmission";

let frames: Map<number, FrameRequestCallback>;
let sequence = 0;

function nextFrame() {
  const callbacks = [...frames.values()];
  frames.clear();
  callbacks.forEach((callback) => callback(sequence));
}

beforeEach(() => {
  frames = new Map();
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    frames.set(++sequence, callback);
    return sequence;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => frames.delete(id));
});
afterEach(() => vi.unstubAllGlobals());

describe("native image admission", () => {
  it("starts two per frame and holds at four until load/decode completes", () => {
    const admit = createImageAdmission();
    const finish: Array<() => void> = [];
    for (let i = 0; i < 20; i++) admit((release) => finish.push(release));
    expect(finish).toHaveLength(0);
    nextFrame();
    expect(finish).toHaveLength(2);
    nextFrame();
    nextFrame();
    expect(finish).toHaveLength(4);
    finish[0]();
    finish[0]();
    nextFrame();
    expect(finish).toHaveLength(5);
    finish[1]();
    finish[2]();
    nextFrame();
    expect(finish).toHaveLength(7);
  });

  it("paces even synchronous cache hits across separate frames", () => {
    const admit = createImageAdmission();
    const start = vi.fn((release: () => void) => release());
    for (let i = 0; i < 10; i++) admit(start);
    for (let frame = 1; frame <= 5; frame++) {
      nextFrame();
      expect(start).toHaveBeenCalledTimes(frame * 2);
    }
  });

  it("drops cancelled queue entries and releases active entries exactly once", () => {
    const admit = createImageAdmission();
    const active = vi.fn();
    const cancel = Array.from({ length: 4 }, () => admit(active));
    nextFrame();
    nextFrame();
    const departed = vi.fn();
    const queued = Array.from({ length: 100 }, () => admit(departed));
    queued.forEach((abort) => abort());
    cancel[0]();
    cancel[0]();
    const replacement = vi.fn();
    admit(replacement);
    nextFrame();
    expect(replacement).toHaveBeenCalledTimes(1);
    expect(departed).not.toHaveBeenCalled();
  });

  it("lets ready sources start before paint and retain four slots when alone", () => {
    const admit = createImageAdmission();
    const start = vi.fn();
    for (let i = 0; i < 20; i++) admit(start, true);
    expect(start).toHaveBeenCalledTimes(2);
    nextFrame();
    nextFrame();
    expect(start).toHaveBeenCalledTimes(4);
  });

  it("leaves two slots for new covers when four returning covers are slow", () => {
    const admit = createImageAdmission();
    const returning = vi.fn();
    for (let i = 0; i < 10; i++) admit(returning, true);
    nextFrame();
    nextFrame();
    expect(returning).toHaveBeenCalledTimes(4);
    const cold = vi.fn();
    for (let i = 0; i < 10; i++) admit(cold);
    nextFrame();
    nextFrame();
    expect(cold).toHaveBeenCalledTimes(2);
    expect(returning).toHaveBeenCalledTimes(4);
  });

  it("admits returning covers while four cold decodes remain pending", () => {
    const admit = createImageAdmission();
    const cold: Array<() => void> = [];
    const ready: Array<() => void> = [];
    for (let i = 0; i < 10; i++) admit((release) => cold.push(release));
    nextFrame();
    nextFrame();
    nextFrame();
    for (let i = 0; i < 10; i++) admit((release) => ready.push(release), true);
    expect(cold).toHaveLength(4);
    expect(ready).toHaveLength(2);
    nextFrame();
    expect(cold.length + ready.length).toBe(6);
    ready[0]();
    ready[0]();
    nextFrame();
    expect(ready).toHaveLength(3);
    expect(cold).toHaveLength(4);
    cold[0]();
    nextFrame();
    expect(cold).toHaveLength(5);
    expect(ready).toHaveLength(3);
  });

  it("shares the frame budget fairly between queued cold and returning covers", () => {
    const admit = createImageAdmission();
    const starts: string[] = [];
    for (let i = 0; i < 10; i++)
      admit((release) => {
        starts.push(`cold-${i}`);
        release();
      });
    for (let i = 0; i < 10; i++)
      admit((release) => {
        starts.push(`ready-${i}`);
        release();
      }, true);
    expect(starts).toHaveLength(0);
    for (let frame = 0; frame < 10; frame++) {
      nextFrame();
      expect(starts.slice(frame * 2)).toEqual([
        `ready-${frame}`,
        `cold-${frame}`,
      ]);
    }
  });

  it("cancels ready queue entries without releasing a cold slot", () => {
    const admit = createImageAdmission();
    const cold = vi.fn();
    for (let i = 0; i < 5; i++) admit(cold);
    nextFrame();
    nextFrame();
    nextFrame();
    const ready = vi.fn();
    const active = [admit(ready, true), admit(ready, true)];
    const cancelled = vi.fn();
    const waiting = Array.from({ length: 10 }, () => admit(cancelled, true));
    waiting.forEach((cancel) => cancel());
    active[0]();
    active[0]();
    const replacement = vi.fn();
    admit(replacement, true);
    nextFrame();
    expect(cold).toHaveBeenCalledTimes(4);
    expect(ready).toHaveBeenCalledTimes(2);
    expect(replacement).toHaveBeenCalledTimes(1);
    expect(cancelled).not.toHaveBeenCalled();
  });
});
