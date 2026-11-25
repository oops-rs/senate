#!/usr/bin/env python3
"""EXPLAIN QUERY PLAN for every SQL statement written in a Rust source file.

    scripts/perf/query_plans.py crates/senate-bridge/src/state.rs /tmp/senate-perf/h/.senate/state.db
    scripts/perf/query_plans.py crates/senate-log/src/sqlite.rs /tmp/senate-perf/h/.senate/senates/top/log.db

Reads the database read-only. Prints each statement that scans a table (no
index narrows it) or sorts with a temporary B-tree, with the table's row
count, largest table first; `--all` prints every plan. A statement built
with `format!` is skipped and counted.
"""

from __future__ import annotations

import re
import sqlite3
import sys

STRING = re.compile(r'r#"(.*?)"#|"((?:[^"\\]|\\.)*)"', re.S)
VERB = re.compile(r"^\s*(SELECT|UPDATE|DELETE|INSERT|WITH|REPLACE)\b", re.I)


def statements(source: str):
    for match in STRING.finditer(source):
        raw = match.group(1)
        if raw is None:
            # A Rust string: a backslash at a line's end joins the lines.
            raw = re.sub(r"\\\n\s*", "", match.group(2)).replace('\\"', '"').replace("\\n", "\n")
        if VERB.match(raw):
            line = source.count("\n", 0, match.start()) + 1
            yield line, " ".join(raw.split())


def main() -> None:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    show_all = "--all" in sys.argv
    if len(args) != 2:
        raise SystemExit(__doc__)
    path, database = args
    source = open(path).read()
    db = sqlite3.connect(f"file:{database}?mode=ro", uri=True)
    rows = {}
    for (name,) in db.execute("SELECT name FROM sqlite_schema WHERE type = 'table'"):
        try:
            rows[name] = db.execute(f'SELECT count(*) FROM "{name}"').fetchone()[0]
        except sqlite3.Error:
            rows[name] = 0
    found = []
    skipped = 0
    total = 0
    for line, sql in statements(source):
        total += 1
        if "{" in sql:
            skipped += 1
            continue
        try:
            plan = [r[3] for r in db.execute(f"EXPLAIN QUERY PLAN {sql}", [None] * sql.count("?"))]
        except sqlite3.Error:
            # Numbered parameters: bind as many as the highest number.
            try:
                n = max([int(x) for x in re.findall(r"\?(\d+)", sql)] or [0])
                plan = [r[3] for r in db.execute(f"EXPLAIN QUERY PLAN {sql}", [None] * n)]
            except sqlite3.Error as error:
                skipped += 1
                if show_all:
                    print(f"{path}:{line}: not explained ({error})")
                continue
        scans = [p for p in plan if p.startswith("SCAN") and "USING" not in p]
        sorts = [p for p in plan if "TEMP B-TREE" in p]
        weight = max([rows.get(p.split()[1], 0) for p in scans] or [0])
        if show_all or scans or sorts:
            found.append((weight, line, sql, plan))
    found.sort(key=lambda f: (-f[0], f[1]))
    for weight, line, sql, plan in found:
        print(f"{path}:{line}  (largest scanned table: {weight} rows)")
        print(f"    {sql[:300]}")
        for step in plan:
            print(f"      {step}")
    print(f"\n{total} statements, {len(found)} listed, {skipped} not explained (built at run time or not valid alone)")


if __name__ == "__main__":
    main()
