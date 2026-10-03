// Bound the browser's load/decode work as well as IPC downloads. Previously
// displayed covers and new sources each have two reserved slots so neither
// can block the other. Alone, either queue retains the original four slots.
// Both queues share the same two-starts-per-frame budget.
const MAX_COLD_ACTIVE = 4;
const MAX_READY_ACTIVE = 4;
const MAX_TOTAL_ACTIVE = 6;
const STARTS_PER_FRAME = 2;

type Start = (release: () => void) => void;
interface Lane {
  waiting: Map<symbol, Start>;
  active: number;
  limit: number;
}

export function createImageAdmission() {
  const cold: Lane = {
    waiting: new Map(),
    active: 0,
    limit: MAX_COLD_ACTIVE,
  };
  const ready: Lane = {
    waiting: new Map(),
    active: 0,
    limit: MAX_READY_ACTIVE,
  };
  let preferred = ready;
  let frame: number | undefined;
  let startedThisFrame = 0;

  const canStart = (lane: Lane) =>
    lane.active < lane.limit &&
    cold.active + ready.active < MAX_TOTAL_ACTIVE &&
    lane.waiting.size > 0;
  const nextLane = () => {
    const other = preferred === ready ? cold : ready;
    if (canStart(preferred)) return preferred;
    if (canStart(other)) return other;
  };
  const startNext = (lane: Lane) => {
    const next = lane.waiting.entries().next().value;
    if (!next) return;
    const [id, start] = next;
    lane.waiting.delete(id);
    lane.active++;
    startedThisFrame++;
    // Alternate whenever both queues can advance, even when a start releases
    // synchronously. A stream of returning covers must not starve new ones.
    preferred = lane === ready ? cold : ready;
    start(() => {
      lane.active--;
      schedule();
    });
  };

  const schedule = () => {
    if (frame !== undefined || (startedThisFrame === 0 && !nextLane())) return;
    frame = requestAnimationFrame(() => {
      frame = undefined;
      startedThisFrame = 0;
      let lane: Lane | undefined;
      while (startedThisFrame < STARTS_PER_FRAME && (lane = nextLane()))
        startNext(lane);
      schedule();
    });
  };

  return (start: Start, wasReady = false) => {
    const lane = wasReady ? ready : cold;
    const id = Symbol();
    let released = false;
    let finish: (() => void) | undefined;
    const release = () => {
      if (released) return;
      released = true;
      lane.waiting.delete(id);
      finish?.();
      if (
        cold.waiting.size === 0 &&
        ready.waiting.size === 0 &&
        startedThisFrame === 0 &&
        frame !== undefined
      ) {
        cancelAnimationFrame(frame);
        frame = undefined;
      }
    };
    lane.waiting.set(id, (done) => {
      finish = done;
      try {
        start(release);
      } catch (error) {
        release();
        throw error;
      }
    });
    // Attach returning artwork before paint when there is no runnable cold
    // queue to arbitrate. Its decode still occupies a bounded reserved slot:
    // WebKit may have discarded the previously displayed decoded surface.
    if (
      wasReady &&
      ready.waiting.size === 1 &&
      canStart(ready) &&
      !canStart(cold) &&
      startedThisFrame < STARTS_PER_FRAME
    )
      startNext(ready);
    schedule();
    return release;
  };
}

export const admitImage = createImageAdmission();

// Previously displayed covers may remount before paint when the budget allows.
// Bound this metadata independently of the browser's HTTP/decoded-image cache.
const readySources = new Map<string, true>();
export function isImageReady(src: string): boolean {
  return readySources.has(src);
}

export function rememberReadyImage(src: string) {
  readySources.delete(src);
  readySources.set(src, true);
  if (readySources.size > 2048)
    readySources.delete(readySources.keys().next().value!);
}
