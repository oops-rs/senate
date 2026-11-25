const MAX_FRAME_BYTES = 64 * 1024;
const MAX_STATE_BYTES = 4 * 1024 * 1024;
const MAX_RETRY_MS = 60_000;
const cursorPattern = /^[A-Za-z0-9_-]{21}[AQgw]$/;
const encoder = new TextEncoder();

export class ObserveError extends Error {
  constructor(code, message, status = 0, retryAfterMs) {
    super(message);
    this.name = "ObserveError";
    this.code = code;
    this.status = status;
    this.retryAfterMs = retryAfterMs;
  }
}

export function endpointUrl(endpoint) {
  const url = new URL(endpoint);
  if (url.protocol !== "http:" || url.pathname !== "/" || url.username || url.password ||
      url.search || url.hash ||
      !(url.hostname === "[::1]" || /^127\.(?:\d{1,3}\.){2}\d{1,3}$/.test(url.hostname))) {
    throw new ObserveError("endpoint", "Use the bound numeric loopback Observe URL.");
  }
  return url;
}

function requestOptions(token, signal, cursor) {
  const headers = new Headers({ authorization: `Bearer ${token}` });
  if (cursor) headers.set("last-event-id", cursor);
  return { headers, signal, cache: "no-store", credentials: "omit", redirect: "error" };
}

function requireMajor(value) {
  if (value?.schema_version !== 1) {
    throw new ObserveError("version_mismatch", "Expected Observe schema version 1.");
  }
}

function requireCursor(cursor) {
  if (typeof cursor !== "string" || !cursorPattern.test(cursor)) {
    throw new ObserveError("invalid_cursor", "Observe returned an invalid cursor.");
  }
}

function failed(response) {
  const status = response.status;
  const value = status === 429 || status >= 500 ? response.headers.get("retry-after")?.trim() : undefined;
  let hint;
  if (value) {
    if (/^[0-9]+$/.test(value)) hint = Math.min(Number(value) * 1000, MAX_RETRY_MS);
    else if (/^[A-Za-z]{3,9},? [0-9A-Za-z]/.test(value)) {
      const date = Date.parse(value);
      if (Number.isFinite(date)) hint = Math.min(Math.max(date - Date.now(), 0), MAX_RETRY_MS);
    }
  }
  return new ObserveError("http", `Observe request returned ${status}.`, status, hint);
}

export async function fetchSnapshot({ endpoint, token, signal }) {
  const url = new URL("/v1/observe/state", endpointUrl(endpoint));
  const response = await fetch(url, requestOptions(token, signal));
  if (!response.ok) throw failed(response);
  const reader = response.body?.getReader();
  if (!reader) throw new ObserveError("invalid_response", "Observe snapshot has no body.");
  const decoder = new TextDecoder("utf-8", { fatal: true });
  let length = 0;
  let text = "";
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      length += value.byteLength;
      if (length > MAX_STATE_BYTES) throw new ObserveError("oversized", "Observe snapshot exceeds its byte limit.");
      text += decoder.decode(value, { stream: true });
    }
    text += decoder.decode();
  } finally {
    await reader.cancel().catch(() => {});
  }
  const snapshot = JSON.parse(text);
  requireMajor(snapshot);
  requireCursor(snapshot.cursor);
  return snapshot;
}

export async function* parseEventStream(body) {
  const reader = body.getReader();
  const decoder = new TextDecoder("utf-8", { fatal: true });
  let pending = "";
  let data = [];
  let id;
  let size = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) return;
      pending += decoder.decode(value, { stream: true });
      for (;;) {
        const match = /\r\n|[\r\n]/.exec(pending);
        if (!match || (match[0] === "\r" && match.index + 1 === pending.length)) break;
        const line = pending.slice(0, match.index);
        pending = pending.slice(match.index + match[0].length);
        size += encoder.encode(line).byteLength + 1;
        if (size > MAX_FRAME_BYTES + 128) throw new ObserveError("oversized", "Observe event exceeds its byte limit.");
        if (line === "") {
          if (data.length > 0) {
            const frame = JSON.parse(data.join("\n"));
            requireMajor(frame);
            if (frame.kind === "stream.reset" && id === undefined && frame.cursor === undefined) {
              yield frame;
            } else {
              requireCursor(frame.cursor);
              if (frame.cursor !== id || typeof frame.kind !== "string") {
                throw new ObserveError("invalid_event", "Observe event and SSE ID disagree.");
              }
              yield frame;
            }
          }
          data = [];
          id = undefined;
          size = 0;
        } else if (line.startsWith("id:")) {
          if (id !== undefined) throw new ObserveError("invalid_event", "Observe sent duplicate SSE IDs.");
          id = line.slice(3).replace(/^ /, "");
        } else if (line.startsWith("data:")) {
          data.push(line.slice(5).replace(/^ /, ""));
        }
      }
      if (pending.length > MAX_FRAME_BYTES) throw new ObserveError("oversized", "Observe SSE line exceeds its byte limit.");
    }
  } finally {
    await reader.cancel().catch(() => {});
  }
}

const key = {
  links: (row) => row.id, committees: (row) => row.id, projects: (row) => row.id,
  senators: (row) => JSON.stringify([row.committee_id, row.id]),
  retired: (row) => JSON.stringify([row.committee_id, row.id]),
  threads: (row) => JSON.stringify([row.committee_id, row.root_seq]),
  desk_items: (row) => JSON.stringify([row.committee_id, row.root_seq]),
  approvals: (row) => row.id,
};

export function applyFrame(state, frame) {
  const next = { ...state, cursor: frame.cursor };
  if (frame.kind !== "state.patch") return next;
  next.as_of = frame.at;
  if (!frame.data || typeof frame.data !== "object") {
    throw new ObserveError("invalid_event", "Observe returned an invalid patch.");
  }
  const { upsert = {}, remove = {}, replace = {} } = frame.data;
  for (const field of ["senate", "windows", "navigation"]) {
    if (Object.hasOwn(replace, field)) next[field] = replace[field];
  }
  for (const [field, identify] of Object.entries(key)) {
    if (!upsert[field] && !remove[field]) continue;
    const rows = new Map((state[field] ?? []).map((row) => [identify(row), row]));
    for (const row of remove[field] ?? []) rows.delete(identify(row));
    for (const row of upsert[field] ?? []) rows.set(identify(row), row);
    next[field] = [...rows.entries()].sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0).map(([, row]) => row);
  }
  return next;
}

function retry(signal, delay, hint = 0) {
  if (signal?.aborted) return Promise.resolve();
  return new Promise((resolve) => {
    const finish = () => {
      clearTimeout(timer);
      signal?.removeEventListener("abort", finish);
      resolve();
    };
    const timer = setTimeout(finish, Math.min(Math.max(delay, hint) + Math.random() * 255, MAX_RETRY_MS));
    signal?.addEventListener("abort", finish, { once: true });
  });
}

export async function* observe(options) {
  const { endpoint, token, signal } = options;
  const url = new URL("/v1/observe/events", endpointUrl(endpoint));
  let state;
  let delay = 200;
  while (!signal?.aborted) {
    let hint;
    try {
      if (!state) {
        state = await fetchSnapshot(options);
        yield { state };
      }
      const response = await fetch(url, requestOptions(token, signal, state.cursor));
      if (response.status === 409) {
        state = undefined;
      } else {
        if (!response.ok || !response.body) throw failed(response);
        for await (const event of parseEventStream(response.body)) {
          if (event.kind === "stream.reset") {
            state = undefined;
            break;
          }
          if (event.cursor === state.cursor) continue;
          const next = applyFrame(state, event);
          yield { state: next, event };
          state = next;
          delay = 200;
        }
      }
    } catch (error) {
      if (signal?.aborted) return;
      if (error instanceof ObserveError &&
          ((error.status >= 400 && error.status < 500 && error.status !== 429) || error.code === "version_mismatch")) throw error;
      if (error instanceof SyntaxError || (error instanceof ObserveError && !error.status)) state = undefined;
      if (error instanceof ObserveError) hint = error.retryAfterMs;
    }
    await retry(signal, delay, hint);
    delay = Math.min(delay * 2, 2_000);
  }
}
