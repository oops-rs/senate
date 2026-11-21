# Draft: Codex gives up on a host when its AAAA query is REFUSED

Draft of a bug report for the Codex CLI, not filed. Whether and where to
file it is Wendell's call: the real case involves an internal gateway, so
this draft uses `gateway.example.com` and documentation addresses
throughout.

---

**Title:** Model requests never connect when the DNS server answers AAAA with REFUSED (A record is fine)

**Version:** codex-cli 0.171.0 (also 0.164.0), Linux x86_64 (Ubuntu 24.04 under WSL 2). A custom `model_providers` entry with `wire_api = "responses"` and a `base_url` on a private host.

**Summary.** Codex resolves the provider's host name with its own resolver,
sending its own A and AAAA queries to the server in `/etc/resolv.conf`.
When that server answers the AAAA query with `REFUSED` (rcode 5), Codex
never opens a connection, even though the A query succeeds. It retries
forever with `ERROR: Reconnecting... waiting for network`. Tools that use
the system resolver (`getaddrinfo`: curl, Node, Python) reach the same
host at once.

**Reproduce.**

1. Use a DNS server that answers `A gateway.example.com` with an address
   and `AAAA gateway.example.com` with `REFUSED`. Some corporate resolvers
   do this. A local `dnsmasq`/`unbound` configured to refuse AAAA for that
   name reproduces it.
2. `~/.codex/config.toml`:
   ```toml
   model_provider = "gw"
   model = "some-model"

   [model_providers.gw]
   name = "custom"
   base_url = "https://gateway.example.com/v1"
   wire_api = "responses"
   requires_openai_auth = true
   ```
3. `codex exec --skip-git-repo-check "say ok" </dev/null`

**Observed.**
- `ERROR: Reconnecting... waiting for network`, repeated.
- With `RUST_LOG=hyper_util=trace,reqwest=trace`, each attempt logs
  `reqwest::connect: starting new connection: https://gateway.example.com/`,
  then `Http::connect`, then about 5 s later `checkout dropped`. It never
  logs a resolved address or a TCP connect.
- `strace -f -e trace=connect,sendto` shows the A and AAAA queries sent
  (`…\0\0\1\0\1` and `…\0\0\34\0\1` to port 53), and no `connect()` to
  port 443 of the gateway. Another host whose AAAA answer is NOERROR with
  no records connects normally from the same process.

**Expected.** As with `getaddrinfo`: a refused or failed AAAA lookup should
not discard a successful A lookup. Codex should connect over IPv4.

**Comparison on the same machine.**

| | A | AAAA | result |
| --- | --- | --- | --- |
| raw query to the resolver | NOERROR, 1 answer | REFUSED | |
| `curl https://gateway.example.com/` | | | HTTP response at once |
| `getent ahosts gateway.example.com` | | | the IPv4 address |
| `codex exec` | | | never connects |
| `codex exec` with `203.0.113.10 gateway.example.com` in `/etc/hosts` | | | works |
| Codex on Windows, same config and network | | | works (it uses the OS resolver) |

**Workaround.** Pin the host's IPv4 address in `/etc/hosts`, or have the
DNS server answer AAAA with NOERROR and no records.
