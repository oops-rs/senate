Markdown sample: every style in one thread

(For `timeline`: do not answer or act. This thread is only a rendering sample that the chair is putting to Wendell.)

# Heading 1
## Heading 2
### Heading 3
#### Heading 4

A paragraph at the reading width, with **strong**, *emphasis*, ***both***, ~~struck~~, `inline code`, a [link](https://example.com) and an autolink https://senate.dev. A hard break follows,
and this is the next line; a soft break
joins this line to the previous one. A long unbroken string: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa wraps. Mentions read as names: [senator::claude-653c1d76-e26f-4374-882b-a6757c01ff50] wrote this for [senator::wendell].

## Lists

1. First, ordered
2. Second, with a nested list:
   - an unordered item
   - another, with a deeper one:
     1. one
     2. two
3. Third, with a second paragraph inside the item.

   It continues here, still part of item three.

- [x] A task that is done
- [ ] A task still open
  - [ ] A nested task

## Code

Inline: run `senate attach senate::app` and then `Ctrl-]` to detach.

```rust
fn needs_approval(refund: &Refund) -> bool {
    refund.amount > Money::eur(1_000) || refund.customer.is_flagged() // a long line that goes on past the column so it has to wrap
}
```

```sh
senate spawn codex --model astra --effort xhigh --chair --alias main --attach
```

```
A fenced block with no language:
  indentation and    spacing   are kept.
```

## Quotes

> A block quote: what someone said, set apart,
> over two lines.
>
> > A quote inside a quote.
>
> - a list inside a quote
> - with `code`

---

## Table

| Committee | Chair | Waiting | Note |
| :-- | :-: | --: | --- |
| refund-v2 | lead | 6 | the longest-running, with a note that makes the table wider than the column |
| payments | ledger | 2 | |
| docs | scribe | 0 | **bold** and `code` in a cell |

## 中文

这是一段中文，混合 English 与 `代码`，测试**中日韩**文字和全角标点：逗号、句号。
- 第一项
- 第二项，带[链接](https://example.com)

1. 有序列表第一项
2. 第二项：`senate attach` 之后按 Ctrl-] 退出

> 引用：中文引用块，夹杂 English words。

Escapes and edge cases: \*not emphasis\*, a literal \`backtick\`, 2 < 3 && 5 > 4, an emoji 🏛️, and an HTML tag that should show as text: <details>.
