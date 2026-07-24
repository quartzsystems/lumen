"use client";

import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { Columns3, Search } from "lucide-react";

/// One column of a [`DataTable`], ported from Quartz Command's inventory
/// tables: a search box and a set of drop-down filters over the top, columns
/// the operator can hide and drag to resize, and a row count.
export interface Column<T> {
  /// Stable identity. Widths and visibility are remembered under it, so
  /// renaming a key resets an operator's layout — treat it as an id, not a
  /// label.
  key: string;
  header: string;
  /// Starting width in pixels. Every column is resizable from its right edge.
  width: number;
  /// The text the search box and the drop-down filters match on, and what a
  /// column with no `render` shows.
  value: (row: T) => string;
  render?: (row: T) => ReactNode;
  /// Offer this column as a drop-down filter, labelled with `header`. The
  /// options are whatever values are actually present.
  filterable?: boolean;
  /// Keep this column out of the Columns menu, and frozen against the right
  /// edge while the rest of the table scrolls under it — the row's controls,
  /// normally, which stay reachable however many columns are shown.
  pinned?: boolean;
  align?: "left" | "right";
}

const MIN_WIDTH = 60;

/// Remembered per table id, so a layout an operator arranged survives a
/// reload. A browser with no storage (or a private window) is not an error:
/// the table simply starts from its defaults every time.
///
/// `set(next, false)` updates without writing — a column resize fires on every
/// mousemove, and one storage write per pixel is not worth the layout being
/// remembered a few hundred milliseconds sooner. `flush` writes what is
/// currently held, which is what the end of a drag calls.
function useStored<T>(key: string, initial: T): [T, (value: T, persist?: boolean) => void, () => void] {
  const [value, setValue] = useState<T>(initial);
  const loaded = useRef(false);
  const latest = useRef(initial);

  useEffect(() => {
    try {
      const stored = window.localStorage.getItem(key);
      if (stored !== null) {
        const parsed = JSON.parse(stored) as T;
        latest.current = parsed;
        setValue(parsed);
      }
    } catch {
      // Nothing to restore; the defaults are already in place.
    }
    loaded.current = true;
  }, [key]);

  const write = useCallback(() => {
    if (!loaded.current) return;
    try {
      window.localStorage.setItem(key, JSON.stringify(latest.current));
    } catch {
      // Storage full or blocked — the layout just does not persist.
    }
  }, [key]);

  const store = useCallback(
    (next: T, persist = true) => {
      latest.current = next;
      setValue(next);
      if (persist) write();
    },
    [write],
  );

  return [value, store, write];
}

export function DataTable<T>({
  id,
  columns,
  rows,
  rowKey,
  title,
  empty = "Nothing to show.",
  searchPlaceholder = "Search…",
  actions,
}: {
  /// Namespaces the remembered column widths and visibility.
  id: string;
  columns: Column<T>[];
  rows: T[];
  rowKey: (row: T) => string;
  /// Rendered as a full-width row above the column headers — the node these
  /// rows belong to, so a clustered table stays readable when there are
  /// several of them stacked up.
  title?: ReactNode;
  empty?: string;
  searchPlaceholder?: string;
  /// Buttons that belong with this table rather than with the page.
  actions?: ReactNode;
}) {
  const [widths, setWidths, saveWidths] = useStored<Record<string, number>>(`${id}.widths`, {});
  const [hidden, setHidden] = useStored<string[]>(`${id}.hidden`, []);
  const [search, setSearch] = useState("");
  const [filters, setFilters] = useState<Record<string, string>>({});
  const [menuOpen, setMenuOpen] = useState(false);

  const shown = columns.filter((column) => !hidden.includes(column.key));
  const widthOf = (column: Column<T>) => widths[column.key] ?? column.width;

  const filtered = useMemo(() => {
    const needle = search.trim().toLowerCase();
    return rows.filter((row) => {
      for (const [key, wanted] of Object.entries(filters)) {
        if (!wanted) continue;
        const column = columns.find((c) => c.key === key);
        if (column && column.value(row) !== wanted) return false;
      }
      if (needle === "") return true;
      // Search covers every column, including hidden ones: an operator
      // looking for a MAC should find it whether or not that column is up.
      return columns.some((column) => column.value(row).toLowerCase().includes(needle));
    });
  }, [rows, columns, filters, search]);

  // Drag state lives in a ref: a resize fires on every mousemove, and going
  // through React state for each one makes the column lag behind the cursor.
  const drag = useRef<{ key: string; startX: number; startWidth: number } | null>(null);

  useEffect(() => {
    const move = (event: MouseEvent) => {
      const current = drag.current;
      if (!current) return;
      const next = Math.max(MIN_WIDTH, current.startWidth + event.clientX - current.startX);
      setWidths({ ...widths, [current.key]: next }, false);
    };
    const up = () => {
      if (drag.current) saveWidths();
      drag.current = null;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
    return () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
    };
  }, [widths, setWidths, saveWidths]);

  const startResize = (event: React.MouseEvent, column: Column<T>) => {
    event.preventDefault();
    drag.current = { key: column.key, startX: event.clientX, startWidth: widthOf(column) };
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
  };

  const filterColumns = columns.filter((column) => column.filterable);
  const activeFilters = Object.values(filters).filter(Boolean).length;

  return (
    <div>
      <div className="qz-toolbar">
        <label className="qz-search">
          <Search size={14} className="text-[var(--qz-fg-4)] flex-shrink-0" />
          <input
            className="qz-search-input"
            placeholder={searchPlaceholder}
            value={search}
            onChange={(event) => setSearch(event.target.value)}
          />
        </label>

        {filterColumns.map((column) => {
          const options = Array.from(
            new Set(rows.map((row) => column.value(row)).filter((value) => value !== "")),
          ).sort();
          return (
            <label key={column.key} className="qz-filter">
              <span className="qz-filter-label">{column.header}</span>
              <select
                className="select qz-filter-select"
                value={filters[column.key] ?? ""}
                onChange={(event) =>
                  setFilters({ ...filters, [column.key]: event.target.value })
                }
              >
                <option value="">All</option>
                {options.map((option) => (
                  <option key={option} value={option}>
                    {option}
                  </option>
                ))}
              </select>
            </label>
          );
        })}

        <div className="flex-1" />

        <div className="relative">
          <button
            type="button"
            className="btn btn-sm"
            onClick={() => setMenuOpen((open) => !open)}
            title="Choose which columns are shown"
          >
            <Columns3 size={14} />
            Columns
          </button>
          {menuOpen && (
            <>
              {/* Click-away, so the menu closes the way every other one does. */}
              <div className="fixed inset-0 z-30" onClick={() => setMenuOpen(false)} />
              <div className="menu qz-columns-menu">
                {columns
                  .filter((column) => !column.pinned)
                  .map((column) => (
                    <label key={column.key} className="menu-item cursor-pointer">
                      <input
                        type="checkbox"
                        checked={!hidden.includes(column.key)}
                        onChange={() =>
                          setHidden(
                            hidden.includes(column.key)
                              ? hidden.filter((key) => key !== column.key)
                              : [...hidden, column.key],
                          )
                        }
                      />
                      {column.header}
                    </label>
                  ))}
              </div>
            </>
          )}
        </div>

        {actions}

        <span className="qz-rowcount">
          {filtered.length === rows.length
            ? `${rows.length} ${rows.length === 1 ? "row" : "rows"}`
            : `${filtered.length} of ${rows.length}`}
          {activeFilters > 0 && (
            <button
              type="button"
              className="qz-clear-filters"
              onClick={() => {
                setFilters({});
                setSearch("");
              }}
            >
              clear
            </button>
          )}
        </span>
      </div>

      <div className="qz-table-wrap">
        <table className="qz-table qz-table-fixed">
          <colgroup>
            {shown.map((column) => (
              <col key={column.key} style={{ width: widthOf(column) }} />
            ))}
          </colgroup>
          <thead>
            {title !== undefined && (
              <tr>
                <th className="qz-table-title" colSpan={shown.length}>
                  {title}
                </th>
              </tr>
            )}
            <tr>
              {shown.map((column) => (
                <th
                  key={column.key}
                  className={column.pinned ? "qz-sticky-right" : undefined}
                  style={{ textAlign: column.align === "right" ? "right" : "left" }}
                >
                  <span className="qz-th-label">{column.header}</span>
                  {!column.pinned && (
                  <span
                    className="qz-resizer"
                    role="separator"
                    aria-orientation="vertical"
                    aria-label={`Resize ${column.header}`}
                    onMouseDown={(event) => startResize(event, column)}
                    onDoubleClick={() => {
                      const next = { ...widths };
                      delete next[column.key];
                      setWidths(next);
                    }}
                  />
                  )}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {filtered.length === 0 && (
              <tr>
                <td colSpan={shown.length} className="qz-table-empty">
                  {rows.length === 0 ? empty : "No rows match the current filters."}
                </td>
              </tr>
            )}
            {filtered.map((row) => (
              <tr key={rowKey(row)}>
                {shown.map((column) => (
                  <td
                    key={column.key}
                    className={column.pinned ? "qz-sticky-right" : undefined}
                    style={{ textAlign: column.align === "right" ? "right" : "left" }}
                  >
                    {column.render ? column.render(row) : column.value(row) || <Dash />}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

/// The one way an empty cell is written, everywhere.
export function Dash() {
  return <span className="qz-dim">—</span>;
}
