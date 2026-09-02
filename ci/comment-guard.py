#!/usr/bin/env python3
"""comment-guard: cap explanatory prose by COMMENT INTENT, not visibility.

Three tiers, keyed on the syntax the author reached for:

  * `//!` / `/*!` — a module (or crate) design doc. UNBOUNDED. There is one per
    file, at the top, documenting the module as a whole, so it can't be used to
    smuggle per-item rationale the way item docs can. Long correctness / design
    essays (e.g. a memory-ordering proof) legitimately live here.
  * `///` / `/** */` — item documentation. Up to DOC_CAP *prose* lines. Fenced
    ``` code blocks do NOT count, so real examples aren't punished. This holds
    whether or not the item is `pub`: a `pub(crate)` fn deserves a real doc
    paragraph just like a `pub` one. Capping `///` here (rather than exempting
    it, as the old guard did — issue #17) is what stops rationale being hidden
    behind a doc sigil, while still leaving room for a genuine paragraph.
  * `//` / `/* */` — a quick internal aside. Up to INLINE_CAP lines. This is the
    shape that sprawls into walls, so it's kept tight: stanzas separated by a
    single blank line count CUMULATIVELY, so one essay can't be split into runs.

Overflow belongs in the module `//!`, or in docs/<topic>.md with a one-line `//`
pointer — both show up in review.

Scans src/ and tests/ under the given root (default cwd). `--selftest` runs
built-in cases. Exit non-zero if any block exceeds its cap.
"""
import re
import sys
from pathlib import Path

INLINE_CAP = 3
DOC_CAP = 8

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
    block comments, and code info. Block comments are tracked across lines.
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


def violations(path):
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    info = _classify(lines)
    n = len(info)
    out = []

    i = 0
    while i < n:
        rec = info[i]
        if rec["comment"] is None:
            i += 1
            continue

        start = i
        if rec["is_doc"]:
            # Doc run: contiguous doc lines. `//!` / `/*!` module docs are
            # UNBOUNDED; any other doc run caps PROSE (fenced ``` excluded) at
            # DOC_CAP, regardless of the documented item's visibility.
            j = i
            prose = 0
            in_fence = False
            module_doc = False
            while j < n and info[j]["comment"] == "doc":
                s = info[j]["s"]
                if s.startswith("//!") or s.startswith("/*!"):
                    module_doc = True
                body = (
                    s[3:] if (s.startswith("///") or s.startswith("//!")) else s
                )
                if _fence_toggle(body):
                    in_fence = not in_fence
                elif not in_fence and body.strip() != "":
                    prose += 1
                j += 1
            if not module_doc and prose > DOC_CAP:
                out.append((start + 1, prose, DOC_CAP, "doc prose"))
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
            f"Inline // asides cap at {INLINE_CAP}; /// item-doc prose caps at "
            f"{DOC_CAP} (fenced examples excluded); //! module docs are unbounded. "
            "Trim, move the rationale into the module //!, or docs/<topic>.md "
            "with a // pointer."
        )
        return 1
    return 0


def _selftest():
    import tempfile

    cases = [
        # (rust source, expected number of violations)
        # short /// doc -> ok
        ("/// short doc\npub fn a() {}\n", 0),
        # /// on a PRIVATE item now gets the doc cap (8), so 4 lines is fine
        ("/// l1\n/// l2\n/// l3\n/// l4\nfn priv_a() {}\n", 0),
        # ...but /// is NOT unlimited: 9 prose lines on a private item fails
        ("".join(f"/// l{k}\n" for k in range(9)) + "fn priv_b() {}\n", 1),
        # 8 prose /// on a pub item -> ok (boundary)
        ("".join(f"/// l{k}\n" for k in range(8)) + "pub fn c() {}\n", 0),
        # 9 prose /// on a pub item -> violation
        ("".join(f"/// l{k}\n" for k in range(9)) + "pub fn d() {}\n", 1),
        # /// with a long fenced example -> example excluded, ok
        (
            "/// summary\n/// # Examples\n/// ```\n"
            + "".join("/// code\n" for _ in range(12))
            + "/// ```\npub fn e() {}\n",
            0,
        ),
        # pub(crate) gets the SAME doc cap as pub (no pub/private split anymore)
        ("".join(f"/// l{k}\n" for k in range(6)) + "pub(crate) fn f() {}\n", 0),
        # bare inline 4 lines -> violation
        ("// a\n// b\n// c\n// d\nlet x = 1;\n", 1),
        # blank-split dodge: 3 + blank + 3 inline -> cumulative -> violation
        ("// a\n// b\n// c\n\n// d\n// e\n// f\nfn z() {}\n", 1),
        # plain /* */ block, 5 lines -> inline cap -> violation
        ("/* one\n two\n three\n four\n five */\nfn q() {}\n", 1),
        # comment-like lines INSIDE a raw string are string data, not comments
        (
            'const H: &str = r##"<style>\n'
            + "".join("/* css note */\n" for _ in range(8))
            + '</style>"##;\nfn h() {}\n',
            0,
        ),
        # //! module doc may run long -> EXEMPT (no cap)
        ("".join(f"//! l{k}\n" for k in range(40)) + "\npub fn m() {}\n", 0),
        # /*! block module doc may run long -> EXEMPT
        ("/*! l0\n" + "".join(f" l{k}\n" for k in range(30)) + "*/\nfn n() {}\n", 0),
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
