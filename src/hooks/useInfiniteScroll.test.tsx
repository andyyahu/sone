import { act, cleanup, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useInfiniteScroll } from "./useInfiniteScroll";

type Page = {
  items: string[];
  totalNumberOfItems: number;
  nextOffset?: number;
};

let intersect: () => void;

beforeEach(() => {
  vi.stubGlobal(
    "IntersectionObserver",
    class {
      constructor(callback: IntersectionObserverCallback) {
        intersect = () =>
          callback(
            [{ isIntersecting: true } as IntersectionObserverEntry],
            this as unknown as IntersectionObserver,
          );
      }
      observe() {}
      disconnect() {}
    },
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function List({
  fetchPage,
}: {
  fetchPage: (offset: number, limit: number) => Promise<Page>;
}) {
  const { items, hasMore, sentinelRef } = useInfiniteScroll({
    fetchPage,
    pageSize: 2,
  });
  return (
    <>
      <output>{items.join(",")}</output>
      {hasMore && <div ref={sentinelRef} />}
    </>
  );
}

describe("infinite scrolling with API offsets", () => {
  it("does not skip server items when user mixes are prepended to page zero", async () => {
    const fetchPage = vi
      .fn<(offset: number, limit: number) => Promise<Page>>()
      .mockResolvedValueOnce({
        items: ["user-mix", "favorite-a", "favorite-b"],
        totalNumberOfItems: 5,
        nextOffset: 2,
      })
      .mockResolvedValueOnce({
        items: ["favorite-c"],
        totalNumberOfItems: 3,
      });
    const { getByRole } = render(<List fetchPage={fetchPage} />);
    await waitFor(() =>
      expect(getByRole("status").textContent).toBe(
        "user-mix,favorite-a,favorite-b",
      ),
    );

    await act(async () => intersect());

    expect(fetchPage.mock.calls).toEqual([
      [0, 2],
      [2, 2],
    ]);
    expect(getByRole("status").textContent).toBe(
      "user-mix,favorite-a,favorite-b,favorite-c",
    );
    await act(async () => intersect());
    expect(fetchPage).toHaveBeenCalledTimes(2);
  });

  it("stops when the API marks a prepended first page as complete", async () => {
    const fetchPage = vi.fn().mockResolvedValue({
      items: ["user-mix", "favorite-a"],
      totalNumberOfItems: 2,
      nextOffset: 2,
    });
    const { getByRole } = render(<List fetchPage={fetchPage} />);
    await waitFor(() =>
      expect(getByRole("status").textContent).toBe("user-mix,favorite-a"),
    );

    await act(async () => intersect());

    expect(fetchPage).toHaveBeenCalledTimes(1);
  });

  it("keeps using item counts for pages without an explicit offset", async () => {
    const fetchPage = vi
      .fn<(offset: number, limit: number) => Promise<Page>>()
      .mockResolvedValueOnce({ items: ["a", "b"], totalNumberOfItems: 3 })
      .mockResolvedValueOnce({ items: ["c"], totalNumberOfItems: 3 });
    const { getByRole } = render(<List fetchPage={fetchPage} />);
    await waitFor(() => expect(getByRole("status").textContent).toBe("a,b"));

    await act(async () => intersect());

    expect(fetchPage).toHaveBeenLastCalledWith(2, 2);
    expect(getByRole("status").textContent).toBe("a,b,c");
  });
});
