-- What the Senate keeps beside its log: facts that are no committee's
-- events (a child Senate registered from below, the secretaries registered
-- for a senator). Keys are namespaced (`child:<name>`,
-- `secretaries:<senator>`); values are JSON.
CREATE TABLE registry (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
