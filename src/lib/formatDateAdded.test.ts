import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const now = Date.UTC(2026, 8, 28, 12);
const day = 86_400_000;
const dateBefore = (days: number) => new Date(now - days * day).toISOString();

beforeEach(() => {
  vi.resetModules();
  vi.useFakeTimers({ toFake: ["Date"] });
  vi.setSystemTime(now);
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe("formatDateAdded", () => {
  it("keeps relative labels at their existing day boundaries", async () => {
    const { formatDateAdded } = await import("./formatDateAdded");
    expect(formatDateAdded(dateBefore(0))).toBe("This week");
    expect(formatDateAdded(dateBefore(7))).toBe("This week");
    expect(formatDateAdded(dateBefore(7 + 1 / day))).toBe("Last week");
    expect(formatDateAdded(dateBefore(14))).toBe("Last week");
    expect(formatDateAdded(dateBefore(14 + 1 / day))).toBe("Last month");
    expect(formatDateAdded(dateBefore(30))).toBe("Last month");
    expect(formatDateAdded(dateBefore(-10))).toBe("Last week");
  });

  it("formats older dates in the default locale with one shared formatter", async () => {
    const dates = [dateBefore(31), dateBefore(400), dateBefore(800)];
    const expected = dates.map((date) =>
      new Date(date).toLocaleDateString(undefined, {
        year: "numeric",
        month: "short",
        day: "numeric",
      }),
    );
    const DateTimeFormat = Intl.DateTimeFormat;
    const formatter = vi
      .spyOn(Intl, "DateTimeFormat")
      .mockImplementation(function (locales, options) {
        return new DateTimeFormat(locales, options);
      });
    const { formatDateAdded } = await import("./formatDateAdded");

    expect(dates.map(formatDateAdded)).toEqual(expected);
    expect(dates.map(formatDateAdded)).toEqual(expected);
    expect(formatter).toHaveBeenCalledTimes(1);
  });

  it("recalculates relative labels as time advances", async () => {
    const { formatDateAdded } = await import("./formatDateAdded");
    const date = dateBefore(7);
    expect(formatDateAdded(date)).toBe("This week");
    vi.setSystemTime(now + day);
    expect(formatDateAdded(date)).toBe("Last week");
  });

  it("handles missing and invalid API dates without throwing or allocating a formatter", async () => {
    const formatter = vi.spyOn(Intl, "DateTimeFormat");
    const { formatDateAdded } = await import("./formatDateAdded");
    expect(formatDateAdded()).toBe("");
    expect(formatDateAdded("")).toBe("");
    expect(formatDateAdded("invalid date")).toBe("Invalid Date");
    expect(formatter).not.toHaveBeenCalled();
  });
});
