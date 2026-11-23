# Markdown specimen

A paragraph at the reading width, with **strong**, *emphasis*, ~~struck~~, `inline code`, a [link](https://example.com) and an autolink https://senate.dev. A hard break follows,  
and this is the next line; a soft break
joins this line to the previous one. A long unbroken string: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa wraps. Mentions read as names: [senator::claude] asked [senator::alice].

## Lists

1. First, ordered
2. Second, with a nested list:
   - an unordered item
   - another, with a deeper one:
     1. one
     2. two
3. Third

- [x] A task that is done
- [ ] A task still open

### Code

```rust
fn needs_approval(refund: &Refund) -> bool {
    refund.amount > Money::eur(1_000) || refund.customer.is_flagged() // a long line that goes on past the column so it has to wrap
}
```

> A block quote: what someone said, set apart,
> over two lines.

---

| Committee | Chair | Waiting | Note |
| :-- | :-: | --: | --- |
| refund-v2 | lead | 6 | the longest-running, with a note that makes the table wider than the column |
| payments | ledger | 2 | |

## 中文

这是一段中文，混合 English 与 `代码`，测试**中日韩**文字和全角标点：逗号、句号。
- 第一项
- 第二项，带[链接](https://example.com)
