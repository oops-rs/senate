// Generated from the Rust Observe schemas. Run npm run generate.

export type Address = {
  "id": string;
  "kind": "senator";
} | {
  "kind": "office";
} | {
  "kind": "external";
} | {
  "kind": "unknown";
};

export type Approval = {
  "asked_at": string;
  "committee_id": string;
  "id": string;
  "link"?: string | null;
  "senator_id": string;
  "tool_kind": ApprovalTool;
};

export type ApprovalAsked = {
  "id": string;
  "senator_id": string;
  "tool_kind": ApprovalTool;
};

export type ApprovalDecided = {
  "decision": ApprovalDecision;
  "id": string;
  "tool_kind": ApprovalTool;
};

export type ApprovalDecision = "allowed" | "denied" | "dismissed" | "unknown";

export type ApprovalTool = "command" | "edit" | "other" | "unknown";

export type Awaiting = "pick" | "addressed" | "away" | "child_offline" | "unknown";

export type Card = {
  "revision": string;
  "updated_at": string;
  "value": Record<string, unknown>;
};

export type CardChanged = {
  "revision": string;
  "subject": CardSubject;
};

export type CardSubject = {
  "committee_id": string;
  "kind": "committee";
} | {
  "committee_id": string;
  "kind": "senator";
  "senator_id": string;
} | {
  "kind": "person";
  "person_id": string;
} | {
  "kind": "unknown";
};

export type ChairChanged = {
  "chair_id"?: string | null;
  "previous_chair_id"?: string | null;
};

export type Committee = {
  "archived": boolean;
  "card"?: Card | null;
  "chair_id"?: string | null;
  "id": string;
  "link"?: string | null;
  "name": string;
  "project_id"?: string | null;
};

export type CommitteeArchived = {
  "archived": boolean;
};

export type CommitteeRef = {
  "committee_id": string;
};

export type Cursor = string;

export type DeskItem = {
  "awaiting": Awaiting;
  "committee_id": string;
  "kind": MessageKind;
  "link"?: string | null;
  "root_seq": Seq;
};

export type ErrorBody = {
  "code": string;
  "message": string;
};

export type ErrorResponse = {
  "error": ErrorBody;
};

export type EventFrame = {
  "at": string;
  "committee_id"?: string | null;
  "cursor": Cursor;
  "link"?: string | null;
  "schema_version": V1;
} & ({
  "data": StatePatch;
  "kind": "state.patch";
} | {
  "data": MessagePosted;
  "kind": "message.posted";
} | {
  "data": MessageLinePublished;
  "kind": "message.line_published";
} | {
  "data": MessageRouted;
  "kind": "message.routed";
} | {
  "data": ThreadStateChanged;
  "kind": "thread.state_changed";
} | {
  "data": SenatorStateChanged;
  "kind": "senator.state_changed";
} | {
  "data": SenatorRef;
  "kind": "senator.joined";
} | {
  "data": SenatorRef;
  "kind": "senator.left";
} | {
  "data": SenatorRef;
  "kind": "senator.started";
} | {
  "data": SenatorRef;
  "kind": "senator.stopped";
} | {
  "data": SenatorRef;
  "kind": "senator.paused";
} | {
  "data": SenatorRef;
  "kind": "senator.resumed";
} | {
  "data": ChairChanged;
  "kind": "committee.chair_changed";
} | {
  "data": CommitteeRef;
  "kind": "committee.created";
} | {
  "data": CommitteeArchived;
  "kind": "committee.archived";
} | {
  "data": ApprovalAsked;
  "kind": "approval.asked";
} | {
  "data": ApprovalDecided;
  "kind": "approval.decided";
} | {
  "data": SenatorRef;
  "kind": "viewer.attached";
} | {
  "data": SenatorRef;
  "kind": "viewer.detached";
} | {
  "data": LinkChanged;
  "kind": "link.changed";
} | {
  "data": ProjectReleaseSeen;
  "kind": "project.release_seen";
} | {
  "data": ToolActivity;
  "kind": "tool.started";
} | {
  "data": ToolActivity;
  "kind": "tool.ended";
} | {
  "data": CardChanged;
  "kind": "card.changed";
});

export type HoldReason = "awaiting_triage" | "busy" | "paused" | "offline" | "no_push" | "retry" | "unknown";

export type Hosting = "here" | "elsewhere" | "unknown";

export type IdKey = {
  "id": string;
};

export type LinkChanged = {
  "id": string;
  "previous_state": LinkState;
  "state": LinkState;
};

export type LinkKind = "parent" | "child" | "paired_machine" | "unknown";

export type LinkState = "up" | "down" | "unknown";

export type MessageKind = "question" | "request" | "answer" | "note" | "decision" | "unknown";

export type MessageLinePublished = {
  "line": string;
  "seq": Seq;
};

export type MessagePosted = {
  "from": Address;
  "line"?: string | null;
  "message_kind": MessageKind;
  "root_seq"?: Seq | null;
  "seq": Seq;
  "to": Array<Address>;
};

export type MessageRouted = {
  "phase": RoutePhase;
  "reason"?: HoldReason | null;
  "route_id": string;
  "seq"?: Seq | null;
  "target"?: Address | null;
};

export type Navigation = {
  "approvals"?: string | null;
  "desk"?: string | null;
  "history"?: string | null;
  "inbox"?: string | null;
  "profile"?: string | null;
};

export type Person = {
  "card"?: Card | null;
  "id": string;
  "link"?: string | null;
  "name": string;
};

export type Project = {
  "id": string;
  "link"?: string | null;
  "name": string;
  "releases"?: Array<ProjectRelease> | null;
};

export type ProjectRelease = {
  "at": string;
  "fixes": number;
  "merges": number;
  "tag": string;
};

export type ProjectReleaseSeen = {
  "project_id": string;
  "release": ProjectRelease;
};

export type Removals = {
  "approvals"?: Array<IdKey>;
  "committees"?: Array<IdKey>;
  "desk_items"?: Array<ThreadKey>;
  "links"?: Array<IdKey>;
  "projects"?: Array<IdKey>;
  "retired"?: Array<SeatKey>;
  "senators"?: Array<SeatKey>;
  "threads"?: Array<ThreadKey>;
};

export type Replacements = {
  "navigation"?: Navigation | null;
  "senate"?: Senate | null;
  "windows"?: Windows | null;
};

export type ResetData = {
  "reason": ResetReason;
};

export type ResetFrame = {
  "at": string;
  "data": ResetData;
  "kind": ResetKind;
  "schema_version": V1;
};

export type ResetKind = "stream.reset";

export type ResetReason = "overrun" | "source_reset" | "policy_changed" | "unknown";

export type RetiredSenator = {
  "alias"?: string | null;
  "card"?: Card | null;
  "committee_id": string;
  "id": string;
  "last_active_at"?: string | null;
  "link"?: string | null;
  "retired_at": string;
  "runtime"?: string | null;
};

export type RoutePhase = "received" | "filed" | "queued" | "held_for_hook" | "released" | "delivered" | "unknown";

export type SeatKey = {
  "committee_id": string;
  "id": string;
};

export type Senate = {
  "desk_owner"?: string | null;
  "desk_workers"?: Array<string>;
  "id": string;
  "link"?: string | null;
  "name": string;
  "person"?: Person | null;
};

export type SenateLink = {
  "id": string;
  "kind": LinkKind;
  "link"?: string | null;
  "name": string;
  "state": LinkState;
};

export type Senator = {
  "alias"?: string | null;
  "card"?: Card | null;
  "committee_id": string;
  "hosted": Hosting;
  "id": string;
  "kind": SenatorKind;
  "last_active_at"?: string | null;
  "link"?: string | null;
  "runtime"?: string | null;
  "state": SenatorState;
  "viewer_attached"?: boolean | null;
};

export type SenatorKind = "person" | "agent" | "unknown";

export type SenatorRef = {
  "senator_id": string;
};

export type SenatorState = "working" | "idle" | "waiting_on_person" | "paused" | "stopped" | "away" | "unknown";

export type SenatorStateChanged = {
  "previous_state": SenatorState;
  "senator_id": string;
  "state": SenatorState;
};

export type Seq = string;

export type Snapshot = {
  "approvals": Array<Approval>;
  "as_of": string;
  "capabilities": Array<string>;
  "committees": Array<Committee>;
  "cursor": Cursor;
  "desk_items": Array<DeskItem>;
  "links": Array<SenateLink>;
  "navigation"?: Navigation | null;
  "projects": Array<Project>;
  "retired"?: Array<RetiredSenator>;
  "schema_version": V1;
  "senate": Senate;
  "senators": Array<Senator>;
  "threads": Array<Thread>;
  "windows": Windows;
};

export type StatePatch = {
  "remove"?: Removals | null;
  "replace"?: Replacements | null;
  "upsert"?: Upserts | null;
};

export type Thread = {
  "addressees": Array<Address>;
  "committee_id": string;
  "kind": MessageKind;
  "link"?: string | null;
  "opener": Address;
  "root_seq": Seq;
  "state": ThreadState;
  "waits_on": WaitsOn;
};

export type ThreadKey = {
  "committee_id": string;
  "root_seq": Seq;
};

export type ThreadState = "processing" | "active" | "suspended" | "archived" | "unknown";

export type ThreadStateChanged = {
  "previous_state": ThreadState;
  "root_seq": Seq;
  "state": ThreadState;
  "waits_on": WaitsOn;
};

export type ToolActivity = {
  "call_id": string;
  "senator_id": string;
  "session_id": string;
  "tool_kind": ToolKind;
};

export type ToolKind = "command" | "edit" | "read" | "search" | "web" | "other" | `mcp:${string}`;

export type Truncation = {
  "committee_id": string;
  "desk_items": boolean;
  "open_threads": boolean;
  "settled_threads": boolean;
};

export type Upserts = {
  "approvals"?: Array<Approval>;
  "committees"?: Array<Committee>;
  "desk_items"?: Array<DeskItem>;
  "links"?: Array<SenateLink>;
  "projects"?: Array<Project>;
  "retired"?: Array<RetiredSenator>;
  "senators"?: Array<Senator>;
  "threads"?: Array<Thread>;
};

export type V1 = 1;

export type WaitsOn = "agents" | "person" | "none" | "unknown";

export type Windows = {
  "desk_items_per_committee": number;
  "open_threads_per_committee": number;
  "settled_threads_per_committee": number;
  "truncated": Array<Truncation>;
};
