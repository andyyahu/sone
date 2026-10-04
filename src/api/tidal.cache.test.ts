import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import {
  clearCache,
  getArtistBio,
  getFavoriteTracks,
  invalidateCache,
  removeTrackFromFavoritesCache,
  searchTidal,
} from "./tidal";
import type { PaginatedTracks, SearchResults } from "../types";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

beforeEach(() => {
  clearCache();
  vi.mocked(invoke).mockReset();
  vi.spyOn(console, "error").mockImplementation(() => {});
});
afterEach(() => {
  clearCache();
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("API cache in-flight sharing", () => {
  it("keeps search keys that collided under the old 32-bit hash independent", async () => {
    const first = { query: "album 2pvu" } as unknown as SearchResults;
    const second = { query: "album d3ea" } as unknown as SearchResults;
    vi.mocked(invoke)
      .mockResolvedValueOnce(first)
      .mockResolvedValueOnce(second);

    expect(await searchTidal("album 2pvu", 50)).toBe(first);
    expect(await searchTidal("album d3ea", 50)).toBe(second);
    expect(await searchTidal("album 2pvu", 50)).toBe(first);
    expect(await searchTidal("album d3ea", 50)).toBe(second);
    expect(invoke).toHaveBeenCalledTimes(2);
  });

  it("evicts by last access and keeps the budget below its 90% target", async () => {
    // Every response estimates to 30 MB. Sharing the payload between responses
    // avoids allocating a complete 150 MB catalog just to exercise eviction.
    const payload = "x".repeat(15 * 1024 * 1024 - 2);
    vi.mocked(invoke).mockResolvedValue(payload);
    for (const id of [1, 2, 3, 4, 5]) await getArtistBio(id);
    await getArtistBio(1); // Move 1 after 5 in recency order.
    await getArtistBio(6); // 2 and 3 must leave to reach the 90% target.
    expect(invoke).toHaveBeenCalledTimes(6);
    for (const id of [1, 4, 5, 6]) await getArtistBio(id);
    expect(invoke).toHaveBeenCalledTimes(6);
    await getArtistBio(2);
    await getArtistBio(3);
    expect(invoke).toHaveBeenCalledTimes(8);
  });

  it("returns oversized responses without caching them or flushing other keys", async () => {
    const oversized = "x".repeat(75 * 1024 * 1024);
    vi.mocked(invoke)
      .mockResolvedValueOnce("Keep me")
      .mockResolvedValue(oversized);
    await getArtistBio(1);
    expect(await getArtistBio(2)).toBe(oversized);
    expect(await getArtistBio(1)).toBe("Keep me");
    expect(await getArtistBio(2)).toBe(oversized);
    expect(invoke).toHaveBeenCalledTimes(3);
  });

  it("coalesces 100 simultaneous cache misses into one IPC call", async () => {
    const pending = deferred<string>();
    vi.mocked(invoke).mockReturnValue(pending.promise);
    const requests = Array.from({ length: 100 }, () => getArtistBio(7));
    expect(invoke).toHaveBeenCalledTimes(1);
    pending.resolve("Biography");
    expect(await Promise.all(requests)).toEqual(Array(100).fill("Biography"));
    expect(await getArtistBio(7)).toBe("Biography");
    expect(invoke).toHaveBeenCalledTimes(1);
  });

  it("keeps different keys independent and leaves unrelated requests shared", async () => {
    const one = deferred<string>();
    const two = deferred<string>();
    vi.mocked(invoke)
      .mockReturnValueOnce(one.promise)
      .mockReturnValueOnce(two.promise);
    const a = getArtistBio(1);
    const b = getArtistBio(2);
    invalidateCache("lyrics");
    const again = getArtistBio(1);
    expect(invoke).toHaveBeenCalledTimes(2);
    one.resolve("One");
    two.resolve("Two");
    expect(await Promise.all([a, b, again])).toEqual(["One", "Two", "One"]);
  });

  it("releases rejected requests so retries can succeed", async () => {
    const pending = deferred<string>();
    vi.mocked(invoke).mockReturnValueOnce(pending.promise);
    const results = Promise.allSettled([getArtistBio(1), getArtistBio(1)]);
    pending.reject(new Error("offline"));
    expect((await results).map((result) => result.status)).toEqual([
      "rejected",
      "rejected",
    ]);
    vi.mocked(invoke).mockResolvedValueOnce("Recovered");
    expect(await getArtistBio(1)).toBe("Recovered");
    expect(invoke).toHaveBeenCalledTimes(2);
  });

  it.each(["artist", "artist-bio:1", "clear"])(
    "detaches stale requests on %s without deleting a newer request",
    async (prefix) => {
      const old = deferred<string>();
      const fresh = deferred<string>();
      vi.mocked(invoke)
        .mockReturnValueOnce(old.promise)
        .mockReturnValueOnce(fresh.promise);
      const before = getArtistBio(1);
      if (prefix === "clear") clearCache();
      else invalidateCache(prefix);
      const after = getArtistBio(1);
      old.resolve("Old account or stale data");
      await before;
      const sharedAfter = getArtistBio(1);
      expect(invoke).toHaveBeenCalledTimes(2);
      fresh.resolve("Current data");
      expect(await Promise.all([after, sharedAfter])).toEqual([
        "Current data",
        "Current data",
      ]);
      expect(await getArtistBio(1)).toBe("Current data");
      expect(invoke).toHaveBeenCalledTimes(2);
    },
  );

  it("does not let a late stale result overwrite a completed fresh response", async () => {
    const old = deferred<string>();
    vi.mocked(invoke)
      .mockReturnValueOnce(old.promise)
      .mockResolvedValueOnce("Fresh");
    const stale = getArtistBio(1);
    invalidateCache("artist");
    expect(await getArtistBio(1)).toBe("Fresh");
    old.resolve("Stale");
    await stale;
    expect(await getArtistBio(1)).toBe("Fresh");
    expect(invoke).toHaveBeenCalledTimes(2);
  });

  it("does not let a pre-logout rejection detach the new account's request", async () => {
    const old = deferred<string>();
    const fresh = deferred<string>();
    vi.mocked(invoke)
      .mockReturnValueOnce(old.promise)
      .mockReturnValueOnce(fresh.promise);
    const before = Promise.allSettled([getArtistBio(1)]);
    clearCache();
    const after = getArtistBio(1);
    old.reject(new Error("old request failed"));
    await before;
    const sharedAfter = getArtistBio(1);
    expect(invoke).toHaveBeenCalledTimes(2);
    fresh.resolve("New account");
    expect(await Promise.all([after, sharedAfter])).toEqual([
      "New account",
      "New account",
    ]);
  });

  it("detaches pending favorite reads when an optimistic mutation occurs", async () => {
    const pending = deferred<PaginatedTracks>();
    const empty: PaginatedTracks = {
      items: [],
      offset: 0,
      limit: 50,
      totalNumberOfItems: 0,
    };
    vi.mocked(invoke)
      .mockReturnValueOnce(pending.promise)
      .mockResolvedValueOnce(empty);
    const stale = getFavoriteTracks(7);
    removeTrackFromFavoritesCache(7, 1);
    expect(await getFavoriteTracks(7)).toEqual(empty);
    pending.resolve({
      ...empty,
      items: [{ id: 1 }],
      totalNumberOfItems: 1,
    } as PaginatedTracks);
    await stale;
    expect(await getFavoriteTracks(7)).toEqual(empty);
    expect(invoke).toHaveBeenCalledTimes(2);
  });

  it("shares a refresh after the existing TTL expires", async () => {
    vi.useFakeTimers();
    vi.mocked(invoke).mockResolvedValueOnce("Before");
    expect(await getArtistBio(1)).toBe("Before");
    vi.setSystemTime(Date.now() + 24 * 60 * 60_000);
    const refresh = deferred<string>();
    vi.mocked(invoke).mockReturnValueOnce(refresh.promise);
    const one = getArtistBio(1);
    const two = getArtistBio(1);
    expect(invoke).toHaveBeenCalledTimes(2);
    refresh.resolve("After");
    expect(await Promise.all([one, two])).toEqual(["After", "After"]);
  });
});
