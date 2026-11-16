-- Search (`LogStore::search`): trigram matching over event bodies.
-- `pg_trgm` needs no tokenizer, so Chinese bodies are searchable as they are.
-- The index serves both the exact pass (`ILIKE` on every term) and the near
-- pass (`<%`, word similarity).
CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE INDEX events_body_trgm_idx ON events USING gin ((envelope->>'body') gin_trgm_ops);
