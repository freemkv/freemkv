# `keydb_fetch` design notes

Rationale and incident history for `src/keydb_fetch.rs`, kept out of the
source so inline comments stay within the comment-guard's line caps.

## Module overview

Keydb save/update lives in `freemkv-keysources` (`KeydbSource::save` /
`KeydbSource::update`), and the verify + atomic-write path is
transport-agnostic: it takes the fetched bytes (or an injected fetch
closure) and never speaks HTTP itself.

The CLI already depends on `ureq` (for the online key service), so the
`update-keys` command supplies this module's `fetch` as the transport —
handling both `http://` and `https://` — and `KeydbSource::update` then
verifies + atomically saves the raw bytes to the resolved keydb path. No
new dependency.

The fetch is hardened the same way as the online key service
(`freemkv-keysources::online`): resolve + SSRF-guard the host immediately
before the request and pin the validated addresses into the agent, follow
zero redirects (so a public URL can't 30x to an internal host), and bound
the connect/read timeouts and the response body size.

## `cap_error`

The two cases are not the same story: an over-large body is a statement
about what the server sent (E8002 "empty or invalid — re-download it"),
while a dead socket is a statement about the network (E8000 "cannot
connect", i.e. retry). `read_capped` used to return the same value for
both, and `fetch` forwarded it, so every mid-download drop was reported as
corrupt content. Separate from `fetch` because `fetch` needs the network
and this does not.

## `CapError`

Two outcomes that a shared `Error::KeydbInvalid` could not tell apart —
see `cap_error`, which is the only place that turns them into the errors
the user sees.

## `read_capped`

Split out of `fetch` because inside it this was untestable: `fetch` does
real network I/O, and the module's own SSRF guard blocks every address a
local test server could bind to (127.0.0.1, ::1), so no test could reach
the cap end-to-end. The whole decompression-bomb defence was therefore
unexercised — every mutant of the limit and its comparison survived. As a
transport-free function it is directly testable with a `Cursor`.

## `hardened_agent`

The agent MUST be built with `Agent::with_parts`: `Agent::new_with_config`
compiles just as happily and then silently uses the default resolver,
which sends the request to live DNS and reopens the rebinding window. That
fault has no symptom short of an actual attack, so it is pinned by the
`hardened_agent_connects_to_the_pinned_address_not_dns` test.

## `PinnedResolver`

Mirrors the pinned resolver in `freemkv-keysources::online` (as this
module's SSRF guard already mirrors that module's).

## Test: `the_keydb_body_read_is_bounded_not_only_the_headers`

The ureq 2 -> 3 port replaced `timeout_read` — which bounded every socket
read — with `timeout_recv_response`, which in ureq 3 bounds the response
headers only. Nothing bounded the body, so a mirror that answered 200 and
then trickled bytes parked the caller forever: the CLI's `update-keys`
hung silently, and the GUI left "Update keydb now" disabled for the life
of the process, because the button is re-enabled only when the worker
posts a result it never posts. Read off the built config rather than
pinned to source text, so it asserts the value ureq will actually use.

## Test: `every_ssrf_disjunct_blocks_on_its_own`

The existing cases all trip several disjuncts at once (a private address
is also not-public, a metadata address is also link-local), so mutating
any single `||` to `&&` still read true and survived. These addresses are
each blocked by exactly one clause, so a broken clause shows up.

## Test: `read_capped_admits_up_to_the_cap_and_rejects_past_it`

Untestable while it lived inside `fetch`, which needs the network and
whose own SSRF guard blocks any address a test server could bind to. So
every mutant of the limit and its comparison survived the run.

## Test: `a_connection_that_dies_mid_body_is_not_an_invalid_keydb`

Both outcomes used to leave `read_capped` as the same error, and `fetch`
forwards that one specially — so a reset, a read timeout or a dropped
socket told the user "the key database is empty or invalid, re-download
it" (E8002), a claim about the server's content, when the right answer
was "could not connect" (E8000) and "try again". The two cases must not
be the same value.

## Test: `the_cgnat_clause_does_not_block_ordinary_public_addresses`

`octets[0] == 100 && (octets[1] & 0xc0) == 0x40` — mutating that `&&` to
`||` survived, because no test used a public address whose second octet
happens to sit in the 64-127 range while the first octet is not 100.
Under `||` those ordinary addresses would silently start being refused.

## Test: `hardened_agent_connects_to_the_pinned_address_not_dns`

The guard tests above prove which addresses are rejected. None of them
makes a connection, so all of them pass even if `hardened_agent` ignores
the pinned addresses entirely and resolves through live DNS — which is
precisely how a rebind gets back in, with no visible symptom. Pin the
agent at a loopback listener this test owns, then ask for a host that
cannot resolve (`.test`, reserved by RFC 6761). Only a consulted resolver
can turn that name into a connection. Touches no network, and bypasses
`fetch` (whose guard blocks loopback by design).
