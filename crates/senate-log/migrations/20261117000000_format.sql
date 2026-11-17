-- The log's format. 1: senators were paths (`senator::alice/claude`). 2:
-- senators are flat names (`senator::claude`). A log is converted from 1 to
-- 2 only when asked (`PgStore::flatten_names`, `senated flatten-log`); a
-- new, empty log is created at 2.
ALTER TABLE log_meta ADD COLUMN format integer NOT NULL DEFAULT 1;
