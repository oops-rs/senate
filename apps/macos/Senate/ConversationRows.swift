import Foundation
import SenateCore

/// One row of a session's conversation as the page draws it. Reasoning
/// never has a row of its own: it rides on the row it led to.
nonisolated struct ConversationRow: Identifiable, Equatable, Sendable {
    enum Kind: Equatable {
        case user(String)
        case assistant(String)
        case tool(ToolCall)
        case todo([TodoLine])
        case notice(level: String, text: String)
        case senate(SenateLine)
        /// Tool calls back to back (two or more, with nothing but reasoning
        /// between them): one row that opens to them, oldest first.
        case tools([ConversationRow])
    }

    /// The seq of the item the row shows: stable while that item is
    /// replaced in place (a tool call completing).
    let id: UInt64
    let kind: Kind
    /// The reasoning that led to it, oldest first; empty for most rows.
    let thinking: [String]
}

nonisolated enum ConversationRows {
    /// The rows for `items` (oldest first), one rule:
    /// - reasoning whose text is empty is dropped;
    /// - reasoning before a tool call goes inside that call (the first
    ///   following one), shown above its output;
    /// - reasoning before an assistant message, with no tool call between,
    ///   goes with that message, behind a "Thinking" disclosure;
    /// - a to-do list, a notice or a Senate message between does not end
    ///   it: the reasoning goes on to the next call or message;
    /// - a message from the person ends it: reasoning before one, with
    ///   nothing it led to, is not shown, nor is reasoning at the end (the
    ///   runtime is still thinking; it shows once something follows).
    static func rows(_ items: [TimelineEntry]) -> [ConversationRow] {
        grouped(ungrouped(items))
    }

    /// Runs of tool calls, back to back, as one group row each: from two
    /// calls up. A group is known by its first call's seq, so it keeps its
    /// identity as the run grows; the reasoning that led into the first
    /// call rides on the group; a to-do, a notice, a Senate message or
    /// anyone's message ends a run.
    static func grouped(_ rows: [ConversationRow]) -> [ConversationRow] {
        var out: [ConversationRow] = []
        out.reserveCapacity(rows.count)
        var run: [ConversationRow] = []
        func flush() {
            if run.count >= 2 {
                var members = run
                members[0] = ConversationRow(id: run[0].id, kind: run[0].kind, thinking: [])
                out.append(ConversationRow(id: run[0].id, kind: .tools(members), thinking: run[0].thinking))
            } else {
                out.append(contentsOf: run)
            }
            run = []
        }
        for row in rows {
            if case .tool = row.kind {
                run.append(row)
            } else {
                flush()
                out.append(row)
            }
        }
        flush()
        return out
    }

    /// The row an item is shown in: its own, a group's member, or (for
    /// reasoning, which has none) the row it led to.
    static func rowId(for seq: UInt64, in rows: [ConversationRow]) -> UInt64? {
        for row in rows {
            if case let .tools(members) = row.kind, let last = members.last, seq <= last.id {
                return members.first { $0.id >= seq }?.id
            }
            if row.id >= seq { return row.id }
        }
        return rows.last?.id
    }

    /// The rows, one per item, before runs of calls are grouped.
    static func ungrouped(_ items: [TimelineEntry]) -> [ConversationRow] {
        var rows: [ConversationRow] = []
        rows.reserveCapacity(items.count)
        var pending: [String] = []
        for item in items {
            switch item.body {
            case let .reasoning(text):
                let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
                if !trimmed.isEmpty { pending.append(trimmed) }
            case let .tool(call):
                rows.append(ConversationRow(id: item.seq, kind: .tool(call), thinking: pending))
                pending = []
            case let .assistant(text):
                rows.append(ConversationRow(id: item.seq, kind: .assistant(text), thinking: pending))
                pending = []
            case let .user(text):
                pending = []
                rows.append(ConversationRow(id: item.seq, kind: .user(text), thinking: []))
            case let .todo(list):
                rows.append(ConversationRow(id: item.seq, kind: .todo(list), thinking: []))
            case let .notice(level, text):
                rows.append(ConversationRow(id: item.seq, kind: .notice(level: level, text: text), thinking: []))
            case let .senate(message):
                rows.append(ConversationRow(id: item.seq, kind: .senate(message), thinking: []))
            }
        }
        return rows
    }
}

