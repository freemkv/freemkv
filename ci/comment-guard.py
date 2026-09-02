#!/usr/bin/env python3
"""comment-guard: cap explanatory prose attached to code by AUDIENCE, not sigil.

The old guard capped only `//` and exempted `///`/`//!`, so a long comment could
be smuggled past the cap just by switching the sigil — and it did (issue #17).
This version keys on who the text is *for*, so the sigil no longer changes the
limit:

  * A doc comment (`///`, `//!`, `/** */`, `/*! */`) on a `pub` item, or a `//!`
    module header, is API documentation: up to PUB_CAP *prose* lines. Fenced
    ``` code blocks inside it do NOT count, so real examples aren't punished.
  * Everything else — a bare `//`, a `/* */` block, or a `///`/`//!` on a
    NON-`pub` item — is an internal comment: up to INLINE_CAP lines. A doc sigil
    on a private item is held to the SAME limit as `//`: the escape hatch is gone.

Anti-dodge:
  * All comment syntaxes are counted (line and block).
  * Internal comment stanzas separated by a single blank line count
    CUMULATIVELY, so one essay can't be broken into short runs.

Overflow belongs in docs/<topic>.md with a one-line `//` pointer — the only
sanctioned way past the cap, and it shows up in review.

Visibility is a heuristic (no rustc): an item is "pub" if its line begins with
an unrestricted `pub ` (NOT `pub(crate)`/`pub(super)`, which don't reach
`cargo doc`), or it is a member of an enclosing `pub` enum/struct/union/trait.
`//!` is always API (module docs). Good enough for a lint; err toward INLINE.

Scans src/ and tests/ under the given root (default cwd). `--selftest` runs
built-in cases. Exit non-zero if any block exceeds its cap.
"""
import re
import sys
from pathlib import Path

INLINE_CAP = 3
PUB_CAP = 8

_TYPE_OPENER = re.compile(r"^(pub(\([^)]*\))?\s+)*(enum|struct|union|trait)\b")
_PUB_UNRESTRICTED = re.compile(r"^pub\s")  # `pub ` — not `pub(crate)` etc.
_ATTR = re.compile(r"^#\[")
_RAW_OPEN = re.compile(r'r(#*)"')  # raw string opener: r"…", r#"…"#, br##"…"## …


def _strip_line_comment(s):
    """Return (kind, body) for a whole-line comment, else (None, None).

    kind is 'doc' for `///`/`//!`, 'plain' for other `//`. A trailing comment on
    a code line is not a whole-line comment and is ignored (single-line, cheap).
    """
    if s.startswith("//!"):
        return "doc", s[3:]
    if s.startswith("///") and not s.startswith("////"):
        return "doc", s[3:]
    if s.startswith("//"):
        return "plain", s[2:]
    return None, None


def _fence_toggle(body):
    """True if this doc body line opens/closes a ``` fenced code block."""
    return body.strip().startswith("```")


def _classify(lines):
    """Yield per-line dicts: comment kind ('doc'/'plain'/None), doc-ness of
    block comments, and code/brace info. Block comments are tracked across lines.
    """
    info = []
    in_block = False
    block_is_doc = False
    raw_hashes = None  # None = not in a raw string; int N = open, needs '"' + N*'#'
    for raw in lines:
        s = raw.lstrip()
        rec = {"raw": raw, "s": s, "comment": None, "is_doc": False, "code": False}
        # Inside a raw string literal (e.g. an embedded HTML/CSS/JS asset): the
        # bytes are string data, not Rust comments, so the guard is blind to them
        # until the string closes. Prevents `/* */` or `//` lines in an embedded
        # page from being counted as comment blocks.
        if raw_hashes is not None:
            if ('"' + "#" * raw_hashes) in raw:
                raw_hashes = None
            info.append(rec)
            continue
        if in_block:
            rec["comment"] = "doc" if block_is_doc else "plain"
            rec["is_doc"] = block_is_doc
            if "*/" in raw:
                in_block = False
            info.append(rec)
            continue
        # A block comment only starts a comment-line if it's the first token.
        if s.startswith("/*"):
            rec["is_doc"] = s.startswith("/**") or s.startswith("/*!")
            rec["comment"] = "doc" if rec["is_doc"] else "plain"
            if "*/" not in s[2:]:
                in_block = True
                block_is_doc = rec["is_doc"]
            info.append(rec)
            continue
        kind, _ = _strip_line_comment(s)
        if kind is not None:
            rec["comment"] = kind
            rec["is_doc"] = kind == "doc"
            info.append(rec)
            continue
        # Code (or blank). Blank = empty after strip.
        rec["code"] = s != ""
        # A code line may OPEN a raw string that spans following lines. Detected
        # only on code (comments were handled above), so `// see r"x"` is safe.
        m = _RAW_OPEN.search(raw)
        if m and ('"' + "#" * len(m.group(1))) not in raw[m.end():]:
            raw_hashes = len(m.group(1))
        info.append(rec)
    return info


def _next_item_is_pub(info, j, type_pub_stack):
    """Given a doc block ending before index j, decide if the documented item is
    public: unrestricted `pub` on the item line, or a member of an enclosing pub
    type. Skips attributes and blank lines."""
    n = len(info)
    while j < n:
        s = info[j]["s"]
        if info[j]["comment"] is not None:
            # Another comment before code — not our item; stop.
            break
        if s == "" or _ATTR.match(s):
            j += 1
            continue
        if _PUB_UNRESTRICTED.match(s):
            return True
        # Member of an enclosing pub enum/struct/union/trait?
        return bool(type_pub_stack and type_pub_stack[-1])
    # No code item follows (e.g. //! or trailing) — treat as pub only for //!,
    # handled by caller; default False here.
    return False


def violations(path):
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    info = _classify(lines)
    n = len(info)
    out = []

    # Brace + pub-type context so a doc on an enum variant / struct field / trait
    # method inside a `pub` type is treated as public.
    type_pub_stack = []  # (depth_at_open, is_pub)
    depth = 0

    def update_context(idx):
        nonlocal depth
        rec = info[idx]
        if rec["comment"] is not None:
            return
        s = rec["s"]
        opens = rec["raw"].count("{")
        closes = rec["raw"].count("}")
        if opens > closes and _TYPE_OPENER.match(s):
            is_pub = bool(_PUB_UNRESTRICTED.match(s)) or bool(
                type_pub_stack and type_pub_stack[-1][1]
            )
            type_pub_stack.append((depth, is_pub))
        depth += opens - closes
        while type_pub_stack and depth <= type_pub_stack[-1][0]:
            type_pub_stack.pop()

    i = 0
    while i < n:
        rec = info[i]
        if rec["comment"] is None:
            update_context(i)
            i += 1
            continue

        # Gather a contiguous comment block of one class boundary. A block is a
        # run of comment lines; for INTERNAL blocks a single blank line bridges
        # (cumulative), code always ends it. Doc vs plain is decided per run.
        start = i
        is_doc_run = rec["is_doc"]
        # A doc run (/// or //! or /** */) is contiguous doc lines only.
        if is_doc_run:
            j = i
            prose = 0
            in_fence = False
            module_doc = False
            while j < n and info[j]["comment"] == "doc":
                s = info[j]["s"]
                if s.startswith("//!"):
                    module_doc = True
                body = (
                    s[3:] if (s.startswith("///") or s.startswith("//!")) else s
                )
                if _fence_toggle(body):
                    in_fence = not in_fence
                elif not in_fence and body.strip() != "":
                    prose += 1
                j += 1
            is_pub = module_doc or _next_item_is_pub(info, j, type_pub_stack)
            cap = PUB_CAP if is_pub else INLINE_CAP
            measure = prose if is_pub else (j - start)
            if measure > cap:
                kind = "pub-doc prose" if is_pub else "private doc"
                out.append((start + 1, measure, cap, kind))
            for k in range(start, j):
                update_context(k)
            i = j
            continue

        # Internal (plain) run, with single-blank bridging (cumulative).
        j = i
        count = 0
        while j < n:
            r = info[j]
            if r["comment"] == "plain":
                count += 1
                j += 1
                continue
            if r["comment"] == "doc":
                break
            if r["comment"] is None and not r["code"]:
                # blank: bridge only a SINGLE blank, else stop
                if j + 1 < n and info[j + 1]["comment"] == "plain":
                    j += 1
                    continue
                break
            break  # code
        if count > INLINE_CAP:
            out.append((start + 1, count, INLINE_CAP, "inline comment"))
        for k in range(start, j):
            update_context(k)
        i = j
    return out


def main(argv):
    if "--selftest" in argv:
        return _selftest()
    root = Path(argv[1]) if len(argv) > 1 else Path.cwd()
    files = sorted(p for d in ("src", "tests") for p in (root / d).rglob("*.rs"))
    total = 0
    for f in files:
        for start, length, cap, kind in violations(f):
            rel = f.relative_to(root)
            print(f"{rel}:{start}: {kind} block is {length} lines (max {cap})")
            total += 1
    if total:
        print(f"\ncomment-guard: {total} comment block(s) exceed their cap.")
        print(
            "Internal comments (incl. /// on a private item) cap at "
            f"{INLINE_CAP}; pub-doc prose caps at {PUB_CAP} (examples excluded). "
            "Trim, or move long rationale to docs/<topic>.md with a // pointer."
        )
        return 1
    return 0


def _selftest():
    import tempfile

    cases = [
        # (rust source, expected number of violations)
        ("/// short pub doc\npub fn a() {}\n", 0),
        # long /// on a PRIVATE item -> capped at 3
        ("/// l1\n/// l2\n/// l3\n/// l4\nfn priv_a() {}\n", 1),
        # long /// on a PUB item -> capped at 8 prose (7 ok)
        ("".join(f"/// l{k}\n" for k in range(7)) + "pub fn b() {}\n", 0),
        # 9 prose lines on pub item -> violation
        ("".join(f"/// l{k}\n" for k in range(9)) + "pub fn c() {}\n", 1),
        # pub doc with a long fenced example -> example excluded, ok
        (
            "/// summary\n/// # Examples\n/// ```\n"
            + "".join("/// code\n" for _ in range(12))
            + "/// ```\npub fn d() {}\n",
            0,
        ),
        # variant doc inside a PUB enum -> pub cap (7 lines ok)
        (
            "pub enum E {\n"
            + "".join(f"    /// v{k}\n" for k in range(6))
            + "    A,\n}\n",
            0,
        ),
        # bare inline 4 lines -> violation
        ("// a\n// b\n// c\n// d\nlet x = 1;\n", 1),
        # blank-split dodge: 3 + blank + 3 internal -> cumulative -> violation
        ("// a\n// b\n// c\n\n// d\n// e\n// f\nfn z() {}\n", 1),
        # block comment 5 lines -> violation
        ("/* one\n two\n three\n four\n five */\nfn q() {}\n", 1),
        # pub(crate) is NOT unrestricted pub -> private cap
        ("/// l1\n/// l2\n/// l3\n/// l4\npub(crate) fn r() {}\n", 1),
        # comment-like lines INSIDE a raw string are string data, not comments
        (
            'const H: &str = r##"<style>\n'
            + "".join("/* css note */\n" for _ in range(8))
            + '</style>"##;\nfn h() {}\n',
            0,
        ),
    ]
    ok = True
    with tempfile.TemporaryDirectory() as d:
        for idx, (src, want) in enumerate(cases):
            p = Path(d) / f"case{idx}.rs"
            p.write_text(src)
            got = len(violations(p))
            flag = "ok" if got == want else "FAIL"
            if got != want:
                ok = False
            print(f"  selftest {idx}: want {want} got {got} [{flag}]")
    print("selftest PASSED" if ok else "selftest FAILED")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
