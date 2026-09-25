import { useQuery } from "@tanstack/react-query";
import { ArrowDownUp, ArrowRight, BookOpen, Clock3, Plus, Search } from "lucide-react";
import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router-dom";
import { api } from "../api/client";
import type { BookSummary } from "../api/types";
import { formatDuration, formatRelative } from "../lib/format";
import { EmptyState, ErrorState, LoadingState } from "../components/StateViews";
import { Badge, Button, PageHeading, ProgressBar, Select } from "../components/ui";

const statusTone: Record<BookSummary["status"], "neutral" | "accent" | "positive" | "danger"> = {
  draft: "neutral",
  ready: "accent",
  processing: "accent",
  completed: "positive",
  failed: "danger",
};

type LibrarySort = "recent" | "title" | "author";

const SORT_KEY = "audiobookai.librarySort";

function readSort(): LibrarySort {
  try {
    const value = localStorage.getItem(SORT_KEY);
    return value === "title" || value === "author" ? value : "recent";
  } catch {
    return "recent";
  }
}

export function sortBooks(books: BookSummary[], sort: LibrarySort, locale: string): BookSummary[] {
  const collator = new Intl.Collator(locale, { sensitivity: "base", numeric: true });
  return [...books].sort((left, right) => {
    if (sort === "title") return collator.compare(left.title, right.title);
    if (sort === "author") {
      // Books without an author follow the named ones; within an author, series order then title.
      if (Boolean(left.author) !== Boolean(right.author)) return left.author ? -1 : 1;
      return collator.compare(left.author ?? "", right.author ?? "")
        || collator.compare(left.series ?? "", right.series ?? "")
        || (left.seriesPosition ?? 0) - (right.seriesPosition ?? 0)
        || collator.compare(left.title, right.title);
    }
    return right.updatedAt.localeCompare(left.updatedAt);
  });
}

export function LibraryPage() {
  const { t, i18n } = useTranslation();
  const [search, setSearch] = useState("");
  const [sort, setSort] = useState<LibrarySort>(readSort);
  const books = useQuery({ queryKey: ["projects"], queryFn: api.books });
  const filtered = useMemo(() => {
    const query = search.trim().toLocaleLowerCase(i18n.language);
    const items = query
      ? (books.data?.items ?? []).filter((book) =>
        [book.title, book.author, book.series].filter(Boolean).some((value) => value!.toLocaleLowerCase(i18n.language).includes(query)),
      )
      : books.data?.items ?? [];
    return sortBooks(items, sort, i18n.language);
  }, [books.data?.items, i18n.language, search, sort]);
  const changeSort = (value: LibrarySort) => {
    setSort(value);
    try {
      localStorage.setItem(SORT_KEY, value);
    } catch {
      // Remembering the order is a convenience only.
    }
  };

  return (
    <div className="page library-page">
      <PageHeading
        eyebrow={t("library.eyebrow")}
        title={t("library.title")}
        subtitle={t("library.subtitle")}
        actions={<Link className="button button-primary button-md" to="/import"><Plus size={17} />{t("library.import")}</Link>}
      />

      {books.isLoading ? <LoadingState label={t("state.loadingLibrary")} /> : null}
      {books.isError ? <ErrorState error={books.error} onRetry={() => void books.refetch()} /> : null}
      {books.data && books.data.items.length === 0 ? (
        <EmptyState
          title={t("library.emptyTitle")}
          detail={t("library.emptyDetail")}
          action={<Link className="button button-primary button-md" to="/import"><BookOpen size={17} />{t("library.emptyAction")}</Link>}
        />
      ) : null}
      {books.data && books.data.items.length > 0 ? (
        <>
          <div className="library-toolbar">
            <label className="search-box">
              <span className="sr-only">{t("common.search")}</span>
              <Search size={17} />
              <input type="search" value={search} onChange={(event) => setSearch(event.target.value)} onKeyDown={(event) => { if (event.key === "Escape") setSearch(""); }} placeholder={t("library.searchPlaceholder")} />
            </label>
            <label className="library-sort">
              <ArrowDownUp size={15} />
              <span className="sr-only">{t("library.sortLabel")}</span>
              <Select value={sort} aria-label={t("library.sortLabel")} onChange={(event) => changeSort(event.target.value as LibrarySort)}>
                <option value="recent">{t("library.sortRecent")}</option>
                <option value="title">{t("library.sortTitle")}</option>
                <option value="author">{t("library.sortAuthor")}</option>
              </Select>
            </label>
            <span className="book-count">{t("library.bookCount", { count: books.data.total ?? books.data.items.length })}</span>
          </div>
          {filtered.length === 0 ? <EmptyState title={t("library.noMatch")} detail={t("library.noMatchDetail")} action={<Button variant="secondary" onClick={() => setSearch("")}>{t("library.clearSearch")}</Button>} /> : (
            <div className="book-grid">
              {filtered.map((book) => (
                <Link key={book.id} to={`/projects/${book.id}/chapters`} className="book-card" aria-label={t("library.openProject", { title: book.title })}>
                  <div className="book-cover">
                    {book.coverUrl ? <img src={book.coverUrl} alt="" /> : <span>{book.title.slice(0, 1).toUpperCase()}</span>}
                    <div className="cover-sheen" />
                  </div>
                  <div className="book-card-body">
                    <div className="space-between book-card-top">
                      <Badge tone={statusTone[book.status]}>{t(`library.${book.status}`)}</Badge>
                      <ArrowRight className="book-arrow" size={17} />
                    </div>
                    <div>
                      <h2>{book.title}</h2>
                      <p className="book-author">{book.author || t("common.unknown")}</p>
                    </div>
                    <div className="book-meta">
                      <span><BookOpen size={14} />{t("library.chapters", { count: book.chapterCount })}</span>
                      {book.durationSeconds ? <span><Clock3 size={14} />{t("library.duration", { value: formatDuration(book.durationSeconds, i18n.language) })}</span> : null}
                    </div>
                    {book.status === "processing" || book.progress > 0 ? (
                      <div className="book-progress">
                        <div className="space-between"><span>{t("library.progressLabel", { value: Math.round(book.progress) })}</span><span>{Math.round(book.progress)}%</span></div>
                        <ProgressBar value={book.progress} label={t("library.progressLabel", { value: Math.round(book.progress) })} />
                      </div>
                    ) : <div className="book-updated">{t("library.updated", { value: formatRelative(book.updatedAt, i18n.language) })}</div>}
                  </div>
                </Link>
              ))}
            </div>
          )}
        </>
      ) : null}
    </div>
  );
}
