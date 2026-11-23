# Runtime glyphs

`RuntimeClaude`, `RuntimeCodex`, `RuntimeAgy` and `RuntimeTerminal` mark a
session's runtime in the sidebar. They are drawn by
`scripts/make-runtime-icons.swift` (our own work, under this repository's
licence) and rendered as templates, in the row's text colour. Each is one
PDF in its own image set: a real mark, if its vendor allows it, replaces a
glyph by swapping that one file.

They are deliberately not the vendors' logos. As checked in November 2026:

| Runtime | Vendor guidance | Why not the logo |
| --- | --- | --- |
| Claude Code (C in a square) | Anthropic trademark guidelines, <https://www.anthropic.com/legal/trademark-guidelines>; Claude Code legal page, <https://code.claude.com/docs/en/legal-and-compliance> | Use only as approved beforehand; no colour changes; other than plain-text references, written permission is required. |
| Codex (X in a circle) | OpenAI brand guidelines, <https://openai.com/brand/> | Use exactly as provided, no added colours; the permission is non-transferable, so it cannot ship in a public repository (Simple Icons removed the mark for this reason, simple-icons#13944). |
| Antigravity (A cut out of a filled square) | Press kit, <https://antigravity.google/press>; Google brand rules, <https://partnermarketinghub.withgoogle.com/brands/google/trademarks-and-terms/trademark-guidelines-for-proper-usage/> | A one-colour icon is published but with no terms for third parties; Google's rules require approved artwork. |

ACP sessions and unknown runtimes get `RuntimeTerminal` (a `>_` prompt).
The runtime's name is always in the row's tooltip and accessibility label,
and in the session page's header.
