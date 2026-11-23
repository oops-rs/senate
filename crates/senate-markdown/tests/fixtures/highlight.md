## Rust
```rust
#[derive(Debug)]
fn render(value: u32) -> &'static str {
    /* outer /* nested */
       comment */
    let sample = r#"Hello
world"#;
    format!("{value:#08x}");
    sample
}
```

## Swift
```swift
@MainActor
func welcome(_ user: String) -> String {
    /* outer /* nested */
       comment */
    return #"""
    Hello, \#(user).
    """#
}
```

## TypeScript
```typescript
interface User { name: string }
const greet = (u: User): string => `hello ${format(
    `user: ${u.name}`
)}!`;
```

## TSX
```tsx
const view = <Panel title={
    format(user.name)
} enabled>
    你好, {user.name}
    <Icon />
</Panel>;
```

## JavaScript
```javascript
export async function load(url) {
    const response = await fetch(url); // JSON endpoint
    return response.json();
}
```

## JSX
```jsx
const button = <Button onClick={() => {
    setCount(count + 1);
}} aria-label="Increase">Increase</Button>;
```

## Python
```python
@cache
def greet(name: str) -> str:
    """Return a greeting
    for this user."""
    return f"Hello, {name}!"  # localized
```

## Go
```go
func greet(name string) string {
    const prefix = `hello
world`
    return fmt.Sprintf("%s, %s", prefix, name)
}
```

## C
```c
#include <stdio.h>
int main(void) {
    /* Hexadecimal float */
    printf("%f\n", 0x1.8p+2);
    return 0;
}
```

## C++
```cpp
template <typename T>
class Gauge {
public:
    T read() const { return T{0xff}; }
};
```

## Java
```java
@Override
public String message() {
    return """
           Hello, world!
           """;
}
```

## Kotlin
```kotlin
data class User(val name: String)
fun hello(user: User) = """
    Hello, ${user.name}!
""".trimIndent()
```

## Ruby
```ruby
def greet(name)
  text = <<~GREETING
    Hello, #{name}!
  GREETING
  puts(text)
end
```

## Shell
```bash
for name in "$@"; do
    cat <<'MESSAGE'
Hello, world
MESSAGE
    printf '%s\n' "$name"
done
```

## JSON
```json
{
    "enabled": true,
    "names": ["one", "two"],
    "threshold": 1.5e-3
}
```

## YAML
```yaml
message: |-
  Hello, world.
  This is literal text.
enabled: true
```

## TOML
```toml
[server]
enabled = true
message = """
Hello, world.
"""
```

## HTML
```html
<article class="note"
         aria-label="Example">
  你好 <strong>世界</strong>
</article>
```

## CSS
```css
.article > p {
  color: var(--fg);
  margin-inline: 1.25rem;
  /* Theme override */
}
```

## SQL
```sql
SELECT id, name
FROM users
WHERE active = TRUE AND name = 'O''Neal';
```

## Diff
```diff
@@ -2,1 +2,1 @@
-old value
+new value
```

## Markdown
```markdown
## Heading
A paragraph with `code`.
- A list item
```

## Dockerfile
```dockerfile
FROM alpine:3.22
COPY entry.sh /entry.sh
RUN chmod +x /entry.sh
ENTRYPOINT ["/entry.sh"]
```

## Makefile
```makefile
# Configuration
include build.mk
test:
	cargo test
```

## C#
```csharp
public async Task<string> GetAsync() {
    return await ReadAsync();
}
```

## Lua
```lua
--[=[ A long
comment ]=]
local function hello(name)
    return "Hello, " .. name
end
```

## Perl
```perl
use strict;
my $message = "Hello";
print($message);
```

## PHP
```php
<?php
function greet(string $name): string {
    return "Hello, " . $name;
}
```

## Scala
```scala
case class User(name: String)
def greet(user: User): String =
  s"Hello, ${user.name}"
```

## XML
```xml
<?xml version="1.0"?>
<ns:message xml:lang="en">
  Hello
</ns:message>
```
