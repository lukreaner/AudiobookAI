import { describe, expect, it } from "vitest";
import type { BookSummary } from "../api/types";
import { sortBooks } from "./LibraryPage";

const book = (id: string, title: string, updatedAt: string, extra: Partial<BookSummary> = {}): BookSummary => ({
  id, title, updatedAt, chapterCount: 1, selectedChapterCount: 1, progress: 0, status: "draft", ...extra,
});

const books = [
  book("a", "Zebra", "2026-08-01T00:00:00Z", { author: "Berg" }),
  book("b", "apple", "2026-08-03T00:00:00Z"),
  book("c", "Band 10", "2026-08-02T00:00:00Z", { author: "Adler", series: "Saga", seriesPosition: 10 }),
  book("d", "Band 2", "2026-07-30T00:00:00Z", { author: "adler", series: "Saga", seriesPosition: 2 }),
];

describe("library sorting", () => {
  it("orders by most recent update first by default", () => {
    expect(sortBooks(books, "recent", "en").map((item) => item.id)).toEqual(["b", "c", "a", "d"]);
  });

  it("orders titles case-insensitively with natural numbers", () => {
    expect(sortBooks(books, "title", "en").map((item) => item.title)).toEqual(["apple", "Band 2", "Band 10", "Zebra"]);
  });

  it("groups by author, then series position, and puts unknown authors last", () => {
    expect(sortBooks(books, "author", "en").map((item) => item.id)).toEqual(["d", "c", "a", "b"]);
  });
});
