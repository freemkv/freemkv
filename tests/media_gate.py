#!/usr/bin/env python3
"""The qa full-disc media gate: which inputs a rip depends on, and whether they are proven.

Commands (all fail closed):
  guards     run guards G1-G5 over a workspace of sibling checkouts
  checkout   fetch the pinned sibling shas into the workspace and path-patch freemkv
  plan       pin C, resolve L_qa, run the guards, fingerprint, look up evidence, decide
  restore    install the plan's L_qa into a build workspace and check it --locked
  seal       confirm a plan belongs to this run and candidate (any attempt)
  release-lock-assert   third-party lock entries must equal between two locks
  fingerprint           print F for a workspace (diagnostic)
  launch-spec           the run-instances arguments for one EC2 leg, from the policy

The design is qa-media-gate-design-v4.md (private notes); the policy is
tests/media-gate-policy.json, which is itself a required input.
"""

import argparse
import base64
import datetime
import fnmatch
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys

SCHEMA = 4
POLICY = Path(__file__).with_name('media-gate-policy.json')
FIRST_PARTY = ('freemkv', 'libfreemkv', 'freemkv-engine', 'freemkv-keysources',
               'freemkv-i18n', 'freemkv-unlock')
SIBLINGS = FIRST_PARTY[1:]
OWNER = 'freemkv'
SHA = re.compile(r'[0-9a-f]{40}')
TARGETS = ('x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl', 'x86_64-apple-darwin',
           'aarch64-apple-darwin', 'x86_64-pc-windows-msvc')
MAX_EVIDENCE = 1 << 20


class GuardError(Exception):
    """A classification guard fired: the plan cannot trust its own path list."""


def load_policy(path=POLICY):
    policy = json.loads(Path(path).read_text())
    if policy.get('schema') != SCHEMA:
        raise ValueError(f'{path}: policy schema must be {SCHEMA}')
    return policy


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()


def sha256(data):
    return hashlib.sha256(data).hexdigest()


# ── Path classification (§2.1) ─────────────────────────────────────────────

def _match(path, patterns):
    """Glob match; a pattern without '/' matches a top-level name only ('*' never crosses '/')."""
    for p in patterns:
        if p.endswith('/**'):
            if path.startswith(p[:-2]):
                return True
        elif '/' not in p:
            if '/' not in path and fnmatch.fnmatchcase(path, p):
                return True
        elif fnmatch.fnmatchcase(path, p) and path.count('/') == p.count('/'):
            return True
    return False


def classify(repo, path, policy):
    """'required', 'manifest' (normalized Cargo.toml), 'root' (crate-root projection) or None."""
    if repo in policy['libraries']:
        if path == 'Cargo.toml':
            return 'manifest'
        if path in policy['required_extra'].get(repo, []):
            return 'required'
        if _match(path, policy['library_required']):
            return 'required'
        if _match(path, policy['library_inert']):
            return None
        return 'required'
    if repo == 'freemkv':
        if path == 'Cargo.toml':
            return 'manifest'
        if path in policy['freemkv_crate_roots']:
            return 'root'
        if path in policy['freemkv_required'] or _match(path, policy['control_plane']):
            return 'required'
    return None


# ── Rust tokenizer (comments and literal contents never become tokens) ────

_RAW = re.compile(r'[bc]?r(#*)"')
_IDENT = re.compile(r'(?:r#)?[A-Za-z_][A-Za-z0-9_]*')
_CHAR = re.compile(r"b?'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]{1,6}\}|.)|[^\\'\n])'")


def tokenize(src):
    """[(kind, value, line)] with kind in ident, str, punct, num. Comments are dropped."""
    toks, i, n, line = [], 0, len(src), 1
    while i < n:
        c = src[i]
        if c == '\n':
            line += 1
            i += 1
        elif c.isspace():
            i += 1
        elif src.startswith('//', i):
            j = src.find('\n', i)
            i = n if j < 0 else j
        elif src.startswith('/*', i):
            depth, i = 1, i + 2
            while i < n and depth:
                if src.startswith('/*', i):
                    depth, i = depth + 1, i + 2
                elif src.startswith('*/', i):
                    depth, i = depth - 1, i + 2
                else:
                    line += src[i] == '\n'
                    i += 1
        elif _RAW.match(src, i):
            m = _RAW.match(src, i)
            end = '"' + m.group(1)
            j = src.find(end, m.end())
            j = n if j < 0 else j
            value = src[m.end():j]
            toks.append(('str', value, line))
            line += value.count('\n')
            i = j + len(end)
        elif c == '"' or src.startswith(('b"', 'c"'), i):
            start = line
            i += 1 if c == '"' else 2
            out = []
            while i < n and src[i] != '"':
                if src[i] == '\\' and i + 1 < n:
                    if src[i + 1] == '\n':
                        line += 1
                        i += 2
                        while i < n and src[i] in ' \t\r\n':
                            line += src[i] == '\n'
                            i += 1
                        continue
                    out.append({'n': '\n', 't': '\t', '\\': '\\', '"': '"', "'": "'", '0': '\0'}
                               .get(src[i + 1], '\\' + src[i + 1]))
                    i += 2
                    continue
                line += src[i] == '\n'
                out.append(src[i])
                i += 1
            toks.append(('str', ''.join(out), start))
            i += 1
        elif _CHAR.match(src, i):
            i = _CHAR.match(src, i).end()
        elif c == "'":
            m = _IDENT.match(src, i + 1)
            i = m.end() if m else i + 1
        elif _IDENT.match(src, i) and not c.isdigit():
            m = _IDENT.match(src, i)
            toks.append(('ident', m.group(0).removeprefix('r#'), line))
            i = m.end()
        elif c.isdigit():
            j = i
            while j < n and (src[j].isalnum() or src[j] == '_'):
                j += 1
            toks.append(('num', src[i:j], line))
            i = j
        elif src.startswith('::', i):
            toks.append(('punct', '::', line))
            i += 2
        else:
            toks.append(('punct', c, line))
            i += 1
    return toks


def _is(tok, kind, value=None):
    return tok is not None and tok[0] == kind and (value is None or tok[1] == value)


def _at(toks, i):
    return toks[i] if 0 <= i < len(toks) else None


def _group_end(toks, i):
    """Index just past the bracket group that opens at toks[i]."""
    pairs = {'(': ')', '[': ']', '{': '}'}
    opener = toks[i][1]
    depth = 0
    for j in range(i, len(toks)):
        if _is(toks[j], 'punct', opener):
            depth += 1
        elif _is(toks[j], 'punct', pairs[opener]):
            depth -= 1
            if depth == 0:
                return j + 1
    return len(toks)


class Module:
    """Token-level facts about one Rust source file."""

    def __init__(self, text):
        self.toks = tokenize(text)
        self.depth = self._mod_depths()

    def _mod_depths(self):
        """Inline-mod nesting depth at every token."""
        toks, depth, stack, opens, brace = self.toks, [], [], set(), 0
        for i, t in enumerate(toks):
            if _is(t, 'ident', 'mod') and _is(_at(toks, i + 1), 'ident') and _is(_at(toks, i + 2), 'punct', '{'):
                opens.add(i + 2)
        for i, t in enumerate(toks):
            if _is(t, 'punct', '{'):
                brace += 1
                if i in opens:
                    stack.append(brace)
            depth.append(len(stack))
            if _is(t, 'punct', '}'):
                if stack and stack[-1] == brace:
                    stack.pop()
                brace -= 1
        return depth

    def attributes_before(self, i):
        """The outer attribute token groups immediately preceding token i (visibility skipped)."""
        toks, attrs, j = self.toks, [], i - 1
        while j >= 0 and _is(toks[j], 'ident') and toks[j][1] in ('pub', 'crate', 'self', 'super', 'in'):
            j -= 1
        while j >= 0 and _is(toks[j], 'punct', ')'):
            k = j
            while k >= 0 and not _is(toks[k], 'punct', '('):
                k -= 1
            j = k - 1
            while j >= 0 and _is(toks[j], 'ident', 'pub'):
                j -= 1
        while j >= 0 and _is(toks[j], 'punct', ']'):
            depth, k = 0, j
            while k >= 0:
                if _is(toks[k], 'punct', ']'):
                    depth += 1
                elif _is(toks[k], 'punct', '['):
                    depth -= 1
                    if depth == 0:
                        break
                k -= 1
            if k < 1 or not _is(toks[k - 1], 'punct', '#'):
                break
            attrs.insert(0, toks[k - 1:j + 1])
            j = k - 2
        return attrs

    def mod_decls(self, top_only=False):
        """[(name, out_of_line, attrs, line)] for every `mod x;` / `mod x {` (top level only if asked)."""
        out = []
        for i, t in enumerate(self.toks):
            if _is(t, 'ident', 'mod') and _is(_at(self.toks, i + 1), 'ident'):
                nxt = _at(self.toks, i + 2)
                if (_is(nxt, 'punct', ';') or _is(nxt, 'punct', '{')) and not (top_only and self.depth[i]):
                    out.append((self.toks[i + 1][1], _is(nxt, 'punct', ';'),
                                self.attributes_before(i), t[2]))
        return out

    def macro_calls(self, names):
        """[(macro, first argument tokens, line)] for `name!(...)` calls."""
        out = []
        for i, t in enumerate(self.toks):
            if _is(t, 'ident') and t[1] in names and _is(_at(self.toks, i + 1), 'punct', '!') \
                    and _at(self.toks, i + 2) is not None and self.toks[i + 2][1] in '([{':
                end = _group_end(self.toks, i + 2)
                out.append((t[1], self.toks[i + 3:end - 1], t[2]))
        return out

    def path_attributes(self):
        """[(value, line)] for `#[path = "..."]`."""
        out = []
        for i, t in enumerate(self.toks):
            if _is(t, 'punct', '#') and _is(_at(self.toks, i + 1), 'punct', '[') \
                    and _is(_at(self.toks, i + 2), 'ident', 'path') and _is(_at(self.toks, i + 3), 'punct', '='):
                v = _at(self.toks, i + 4)
                out.append((v[1] if _is(v, 'str') else None, t[2]))
        return out

    def use_trees(self):
        """[(leaf names, root segment, line)] for every `use` item."""
        out = []
        for i, t in enumerate(self.toks):
            if not _is(t, 'ident', 'use') or _is(_at(self.toks, i - 1), 'punct', '::'):
                continue
            j = i + 1
            while j < len(self.toks) and not _is(self.toks[j], 'punct', ';'):
                j += 1
            body = self.toks[i + 1:j]
            leaves, prev = [], None
            for k, b in enumerate(body):
                if _is(b, 'ident', 'as') and _is(_at(body, k + 1), 'ident'):
                    if leaves and leaves[-1] == prev:
                        leaves.pop()
                    leaves.append(body[k + 1][1])
                    prev = None
                    continue
                nxt = _at(body, k + 1)
                if _is(b, 'ident') and (nxt is None or _is(nxt, 'punct', ',') or _is(nxt, 'punct', '}')
                                        or _is(nxt, 'ident', 'as')):
                    if not (k > 0 and _is(body[k - 1], 'ident', 'as')):
                        leaves.append(b[1])
                        prev = b[1]
            if body and _is(body[0], 'punct', '::'):
                body = body[1:]
            root = body[0][1] if body and _is(body[0], 'ident') else None
            if not any(_is(b, 'punct', '::') for b in body):
                # `use foo;` / `use foo as bar;` names a crate: only the alias is local.
                leaves = [body[2][1]] if len(body) == 3 and _is(body[1], 'ident', 'as') else []
            out.append((leaves, root, t[2]))
        return out

    def path_starts(self):
        """[(segment, index)] for the first segment of every `a::b` path."""
        out = []
        for i, t in enumerate(self.toks):
            if not _is(t, 'ident') or not _is(_at(self.toks, i + 1), 'punct', '::'):
                continue
            if t[1] in KEYWORDS:
                continue
            prev = _at(self.toks, i - 1)
            if _is(prev, 'punct', '::'):
                # `::x::` is a crate path only when the `::` leads (after a keyword or punctuation).
                before = _at(self.toks, i - 2)
                if (_is(before, 'ident') and before[1] not in KEYWORDS) or _is(before, 'punct', '>'):
                    continue
            elif _is(prev, 'punct', '.') or _is(prev, 'punct', '$'):
                continue
            if _is(_at(self.toks, i + 2), 'punct', '<'):
                continue
            out.append((t[1], i))
        return out


# ── Guards (§2.3) ──────────────────────────────────────────────────────────

INCLUDES = ('include', 'include_str', 'include_bytes')
KEYWORDS = {'use', 'pub', 'in', 'as', 'where', 'impl', 'for', 'dyn', 'return', 'let', 'mut', 'ref', 'if',
            'match', 'else', 'while', 'loop', 'move', 'unsafe', 'fn', 'struct', 'enum', 'type', 'const',
            'static', 'mod', 'trait', 'extern', 'break', 'continue', 'async', 'await'}
BUILTIN_SEGMENTS = {'std', 'core', 'alloc', 'crate', 'self', 'super', 'Self', 'clippy', 'rustfmt',
                    'rustdoc', 'bool', 'char', 'str',
                    'u8', 'u16', 'u32', 'u64', 'u128', 'usize', 'i8', 'i16', 'i32', 'i64', 'i128',
                    'isize', 'f32', 'f64'}


def _rel(base, target):
    """Normalize `target` relative to directory `base` (both repo-relative posix); None if it escapes."""
    parts = []
    for part in (PurePosixPath(base) / target).parts:
        if part == '..':
            if not parts:
                return None
            parts.pop()
        elif part not in ('.', ''):
            parts.append(part)
    return '/'.join(parts)


def _required_rs(repo, files, policy):
    return [f for f in files if f.endswith('.rs') and classify(repo, f, policy) == 'required']


def guard_crate_root(ws, policy):
    """G1: the binary root declares every required module plainly; the lib root, where it declares one, too."""
    errors = []
    for n, root in enumerate(policy['freemkv_crate_roots']):
        mod = Module((ws / 'freemkv' / root).read_text())
        decls = {}
        for name, ool, attrs, line in mod.mod_decls(top_only=True):
            if name in decls:
                errors.append(f'G1 freemkv/{root}:{line}: `mod {name}` declared twice')
            decls[name] = (ool, attrs, line)
        for name in policy['freemkv_modules']:
            if name not in decls:
                if n > 0:
                    continue
                errors.append(f'G1 freemkv/{root}: required module `{name}` is not declared '
                              f'(remedy: declare `mod {name};`, or update the policy)')
                continue
            ool, attrs, line = decls[name]
            if not ool:
                errors.append(f'G1 freemkv/{root}:{line}: `mod {name}` must be out-of-line (`mod {name};`)')
            for attr in attrs:
                head = attr[2][1] if len(attr) > 2 else ''
                if head in ('path', 'cfg', 'cfg_attr'):
                    errors.append(f'G1 freemkv/{root}:{line}: `#[{head}…]` on required `mod {name}` '
                                  '(remedy: a plain `mod x;` — moved or cfg-gated rip code needs a policy change)')
    for path in policy['freemkv_required']:
        if not path.endswith('.rs') or not path.startswith('src/'):
            continue
        mod = Module((ws / 'freemkv' / path).read_text())
        for name, ool, _, line in mod.mod_decls():
            if ool:
                errors.append(f'G1 freemkv/{path}:{line}: out-of-line `mod {name};` in a required file '
                              '(remedy: inline it, or add its file to the required list)')
        for value, line in mod.path_attributes():
            errors.append(f'G1 freemkv/{path}:{line}: `#[path = {value!r}]` in a required file')
        for name, _, line in mod.macro_calls(('include',)):
            errors.append(f'G1 freemkv/{path}:{line}: `include!` in a required file')
    return errors


def guard_inclusions(ws, policy, files_by_repo):
    """G2: every include*!/#[path] target of a required .rs file is itself required."""
    errors = []
    for repo in list(policy['libraries']) + ['freemkv']:
        files = files_by_repo[repo]
        build_required = classify(repo, 'build.rs', policy) == 'required'
        for path in _required_rs(repo, files, policy):
            mod = Module((ws / repo / path).read_text())
            base = str(PurePosixPath(path).parent)
            targets = [(v, line, '#[path]') for v, line in mod.path_attributes()]
            for macro, args, line in mod.macro_calls(INCLUDES):
                lit = args[0][1] if len(args) == 1 and _is(args[0], 'str') else None
                if lit is None:
                    generated = any(_is(a, 'str', 'OUT_DIR') for a in args)
                    if generated and build_required:
                        continue
                    errors.append(f'G2 {repo}/{path}:{line}: `{macro}!` with a non-literal target '
                                  '(remedy: a literal path, or OUT_DIR output of a required build.rs)')
                    continue
                targets.append((lit, line, f'{macro}!'))
            for value, line, what in targets:
                target = _rel(base, value) if value is not None else None
                if target is None or classify(repo, target, policy) != 'required':
                    errors.append(f'G2 {repo}/{path}:{line}: {what} target {value!r} → {target or "outside the repo"} '
                                  'is not a required path (remedy: add it to required_extra, or move it under src/)')
    return errors


_RERUN = re.compile(r'cargo::?rerun-if-changed=(.*)')
_EMITS_CODE = re.compile(r'cargo::?rustc-(cfg|env|check-cfg)=|OUT_DIR')


def guard_build_scripts(ws, policy, files_by_repo):
    """G3: build.rs inputs are required (or exempt with a reason); a code-emitting build.rs is required."""
    errors = []
    for repo in list(policy['libraries']) + ['freemkv']:
        if 'build.rs' not in files_by_repo[repo]:
            continue
        mod = Module((ws / repo / 'build.rs').read_text())
        strings = [(t[1], t[2]) for t in mod.toks if t[0] == 'str']
        if classify(repo, 'build.rs', policy) != 'required':
            for value, line in strings:
                if _EMITS_CODE.search(value):
                    errors.append(f'G3 {repo}/build.rs:{line}: emits compiled code ({value!r}) but build.rs '
                                  'is not required (remedy: make build.rs required)')
            continue
        exempt = policy['rerun_exempt'].get(repo, {})
        for value, line in strings:
            m = _RERUN.match(value)
            if not m:
                continue
            target = re.sub(r'\{[^}]*\}', '*', m.group(1))
            norm = _rel('.', target)
            if norm in files_by_repo[repo] and classify(repo, norm, policy) == 'required':
                continue
            if norm is not None and any(fnmatch.fnmatchcase(norm, pat) for pat in exempt):
                continue
            errors.append(f'G3 {repo}/build.rs:{line}: rerun-if-changed={m.group(1)} is not a required path '
                          f'(remedy: make it required, or add it to rerun_exempt["{repo}"] with a reason)')
    return errors


def guard_call_graph(ws, policy):
    """G4: required freemkv files reach only required modules and allowed crate callees."""
    allowed = set(policy['freemkv_modules']) | set(policy['allowed_crate_callees'])
    errors = []
    for path in policy['freemkv_required']:
        if not path.endswith('.rs') or not path.startswith('src/'):
            continue
        mod = Module((ws / 'freemkv' / path).read_text())
        toks = mod.toks
        # The binary's `crate`, its lib crate `freemkv`, and any alias of either.
        roots = {'crate', 'freemkv'}
        for i, t in enumerate(toks):
            if _is(t, 'ident', 'use') or (_is(t, 'ident', 'crate') and _is(_at(toks, i - 1), 'ident', 'extern')):
                k = i + 1
                if _is(_at(toks, k), 'punct', '::'):
                    k += 1
                if _is(_at(toks, k), 'ident') and toks[k][1] in ('crate', 'freemkv') \
                        and _is(_at(toks, k + 1), 'ident', 'as') and _is(_at(toks, k + 2), 'ident'):
                    roots.add(toks[k + 2][1])
        for i, t in enumerate(toks):
            if not _is(t, 'ident') or not (t[1] in roots or t[1] == 'super'):
                continue
            if not _is(_at(toks, i + 1), 'punct', '::'):
                continue
            prev = _at(toks, i - 1)
            if _is(prev, 'punct', '::'):
                before = _at(toks, i - 2)
                if t[1] != 'freemkv' or _is(before, 'ident') or _is(before, 'punct', '>'):
                    continue
            elif _is(prev, 'punct', '.') or (_is(prev, 'punct', '$') and t[1] != 'crate'):
                continue
            j = i
            if t[1] == 'super' and 'super' not in roots:
                ups = 0
                while _is(_at(toks, j), 'ident', 'super') and _is(_at(toks, j + 1), 'punct', '::'):
                    ups, j = ups + 1, j + 2
                if mod.depth[i] - ups >= 0:
                    continue
            else:
                j = i + 2
            nxt = _at(toks, j)
            if _is(nxt, 'punct', '{'):
                end = _group_end(toks, j)
                names, depth = [], 0
                for k in range(j + 1, end - 1):
                    if toks[k][1] in '{([':
                        depth += 1
                    elif toks[k][1] in '})]':
                        depth -= 1
                    elif depth == 0 and _is(toks[k], 'ident') and not _is(_at(toks, k - 1), 'punct', '::'):
                        if _is(_at(toks, k - 1), 'ident', 'as'):
                            continue
                        names.append('* (self re-exported under an alias)' if toks[k][1] == 'self'
                                     and _is(_at(toks, k + 1), 'ident', 'as') else toks[k][1])
            elif _is(nxt, 'ident'):
                names = [nxt[1]]
            else:
                names = ['*']
            for name in names:
                if name not in allowed and name != 'self':
                    errors.append(f'G4 freemkv/{path}:{t[2]}: required code calls crate::{name} — classify `{name}`: '
                                  'add it to the required list (the default for moved rip code) or to '
                                  'allowed_crate_callees with a reason')
    return errors


def guard_crate_tokens(ws, policy, k_names):
    """G5: external crate paths in required freemkv files are first-party, in K, or closure roots."""
    known = {n.replace('-', '_') for n in list(FIRST_PARTY) + list(k_names) + list(policy['closure_roots'])}
    errors = []
    for path in policy['freemkv_required']:
        if not path.endswith('.rs') or not path.startswith('src/'):
            continue
        mod = Module((ws / 'freemkv' / path).read_text())
        local = set(BUILTIN_SEGMENTS) | {name for name, *_ in mod.mod_decls()}
        for leaves, _, _ in mod.use_trees():
            local.update(leaves)
        seen = set()
        starts = mod.path_starts()
        toks = mod.toks
        for i, t in enumerate(toks):
            if _is(t, 'ident', 'use') and not _is(_at(toks, i - 1), 'punct', '::'):
                k = i + 2 if _is(_at(toks, i + 1), 'punct', '::') else i + 1
                nxt = _at(toks, k + 1)
                if _is(_at(toks, k), 'ident') and toks[k][1] in BUILTIN_SEGMENTS:
                    continue
                if _is(_at(toks, k), 'ident') and (_is(nxt, 'punct', ';') or _is(nxt, 'ident', 'as')):
                    starts.append((toks[k][1], k))
        for seg, i in starts:
            if seg in local or seg in known or seg[:1].isupper() or seg in seen:
                continue
            seen.add(seg)
            errors.append(f'G5 freemkv/{path}:{mod.toks[i][2]}: `{seg}::` is not a first-party crate, in K, '
                          'or a closure root (remedy: add the crate to closure_roots, which puts it in K)')
    return errors


def guard_manifest_targets(ws, policy, files_by_repo):
    """G6: build.rs, the lib and the CLI bin are where the required list expects them;
    no tracked cargo or rustup config can change the freemkv build."""
    errors = []
    for repo in list(policy['libraries']) + ['freemkv']:
        manifest = read_toml((ws / repo / 'Cargo.toml').read_text())
        build = manifest.get('package', {}).get('build')
        if build not in (None, True, False) and (build not in files_by_repo[repo]
                                                 or classify(repo, build, policy) != 'required'):
            errors.append(f'G6 {repo}/Cargo.toml: package.build = {build!r} is not a required build script')
        lib = manifest.get('lib', {}).get('path')
        if repo == 'freemkv':
            if lib not in (None, 'src/lib.rs'):
                errors.append(f'G6 freemkv/Cargo.toml: lib.path = {lib!r} (the projection reads src/lib.rs)')
            if build not in (None, True, 'build.rs'):
                errors.append(f'G6 freemkv/Cargo.toml: package.build = {build!r} (the required build script is build.rs)')
            gui = set(policy['gui_features'])
            for b in manifest.get('bin', []):
                if not gui & set(b.get('required-features', [])) and b.get('path', 'src/main.rs') != 'src/main.rs':
                    errors.append(f'G6 freemkv/Cargo.toml: CLI bin {b.get("name")!r} is built from {b.get("path")!r}, '
                                  'not src/main.rs (remedy: update the crate-root list in the policy)')
            for path in files_by_repo[repo]:
                if path.startswith('.cargo/') or PurePosixPath(path).name in ('rust-toolchain', 'rust-toolchain.toml'):
                    errors.append(f'G6 freemkv/{path}: a tracked cargo/rustup config would change the build '
                                  'outside the fingerprint (remedy: remove it; toolchain and flags live in Cargo.toml)')
        elif lib is not None and classify(repo, lib, policy) != 'required':
            errors.append(f'G6 {repo}/Cargo.toml: lib.path = {lib!r} is not a required path')
    return errors


def run_guards(ws, policy, lock_text):
    files = {repo: git_files(ws / repo) for repo in list(policy['libraries']) + ['freemkv']}
    k_names = {entry[0] for entry in closure(parse_lock(lock_text), policy)}
    errors = (guard_crate_root(ws, policy) + guard_inclusions(ws, policy, files)
              + guard_build_scripts(ws, policy, files) + guard_call_graph(ws, policy)
              + guard_crate_tokens(ws, policy, k_names) + guard_manifest_targets(ws, policy, files))
    return errors


# ── Manifests and locks (§2.4) ─────────────────────────────────────────────

DEP_TABLES = ('dependencies', 'build-dependencies')
PIN_FIELDS = ('version', 'path', 'git', 'tag', 'branch', 'rev')
INERT_PACKAGE = ('version', 'description', 'license', 'license-file', 'authors', 'repository', 'homepage',
                 'documentation', 'readme', 'keywords', 'categories', 'metadata', 'exclude', 'include')


def _strip_first_party(table):
    for name, spec in list(table.items()):
        package = spec.get('package', name) if isinstance(spec, dict) else name
        if package in FIRST_PARTY:
            spec = {k: v for k, v in spec.items() if k not in PIN_FIELDS} if isinstance(spec, dict) else {}
            table[name] = spec


def _each_dep_table(manifest):
    for key in DEP_TABLES:
        if key in manifest:
            yield manifest, key
    for target in manifest.get('target', {}).values():
        for key in DEP_TABLES:
            if key in target:
                yield target, key
    for table in manifest.get('patch', {}).values():
        yield {'_': table}, '_'
    if 'dependencies' in manifest.get('workspace', {}):
        yield manifest['workspace'], 'dependencies'


def _drop_dev(manifest):
    manifest.pop('dev-dependencies', None)
    for target in manifest.get('target', {}).values():
        target.pop('dev-dependencies', None)


def normalize_library_manifest(manifest):
    manifest.get('package', {}).pop('version', None)
    _drop_dev(manifest)
    for owner, key in _each_dep_table(manifest):
        _strip_first_party(owner[key])
    return manifest


def project_freemkv_manifest(manifest, policy):
    """Decision 10's denylist projection: everything but inert metadata and the GUI build."""
    package = manifest.get('package', {})
    for key in INERT_PACKAGE:
        package.pop(key, None)
    gui = set(policy['gui_features'])
    features = manifest.get('features', {})
    gui_deps = set()
    for name in gui:
        gui_deps.update(v[4:] for v in features.pop(name, []) if v.startswith('dep:'))
    for values in features.values():
        gui_deps -= {v[4:] if v.startswith('dep:') else v.split('/')[0].rstrip('?') for v in values}
    manifest['bin'] = [b for b in manifest.get('bin', [])
                       if not gui & set(b.get('required-features', []))]
    _drop_dev(manifest)
    for owner, key in _each_dep_table(manifest):
        for dep in list(owner[key]):
            if dep in gui_deps:
                del owner[key][dep]
        _strip_first_party(owner[key])
    for target in list(manifest.get('target', {})):
        manifest['target'][target] = {k: v for k, v in manifest['target'][target].items() if v}
        if not manifest['target'][target]:
            del manifest['target'][target]
    return manifest


def read_toml(text):
    import tomllib
    return tomllib.loads(text)


def parse_lock(text):
    return read_toml(text).get('package', [])


def _dep_ref(dep):
    parts = dep.split()
    return parts[0], (parts[1] if len(parts) > 1 else None)


def normalize_lock(packages):
    """Sorted third-party + version-stripped first-party entries, for comparison."""
    out = []
    for p in packages:
        p = dict(p)
        if p['name'] in FIRST_PARTY:
            for field in ('version', 'source', 'checksum'):
                p.pop(field, None)
        if 'dependencies' in p:
            p['dependencies'] = sorted(_dep_ref(d)[0] if _dep_ref(d)[0] in FIRST_PARTY else d
                                       for d in p['dependencies'])
        out.append(p)
    return sorted(out, key=canonical)


def third_party(packages):
    return [p for p in normalize_lock(packages) if p['name'] not in FIRST_PARTY]


def closure(packages, policy):
    """K(L): third-party packages reachable from the libraries and closure roots (not via unlock/i18n)."""
    by_name = {}
    for p in packages:
        by_name.setdefault(p['name'], []).append(p)
    skip = set(policy['closure_skip'])
    todo = [p for name in list(policy['libraries']) + list(policy['closure_roots'])
            for p in by_name.get(name, [])]
    seen, out = set(), set()
    while todo:
        p = todo.pop()
        key = (p['name'], p.get('version'), p.get('source'))
        if key in seen or p['name'] in skip:
            continue
        seen.add(key)
        if p['name'] not in FIRST_PARTY:
            out.add((p['name'], p.get('version', ''), p.get('source', ''), p.get('checksum', '')))
        for dep in p.get('dependencies', []):
            name, version = _dep_ref(dep)
            todo.extend(c for c in by_name.get(name, []) if version is None or c.get('version') == version)
    return sorted([list(e) for e in out])


_TREE = re.compile(r'^(\S+) v(\S+)((?: \([^)]*\))*) ?(\S*)(?: \(\*\))?$')


def parse_tree(text, keep):
    """cargo tree -f '{p} {f}' → sorted unique [name, version, features] for packages in `keep`."""
    out = set()
    for line in text.splitlines():
        m = _TREE.match(line.strip())
        if not m or m.group(1) not in keep:
            continue
        name = m.group(1)
        version = '' if name in FIRST_PARTY else m.group(2)
        out.add((name, version, ','.join(sorted(f for f in m.group(4).split(',') if f))))
    return sorted([list(e) for e in out])


def resolved_features(root, k_names, run=subprocess.run):
    """Φ(L): per shipped CLI target, the enabled features of K and the first-party crates."""
    keep = set(k_names) | set(FIRST_PARTY)
    out = {}
    for target in TARGETS:
        res = run(['cargo', 'tree', '--locked', '-e', 'normal,build', '--target', target, '--prefix', 'none',
                   '-f', '{p} {f}'], cwd=root, capture_output=True, text=True, check=True)
        out[target] = parse_tree(res.stdout, keep)
    return out


# ── Crate-root projection ──────────────────────────────────────────────────

def project_crate_root(text, policy):
    """Inner attributes, the #[global_allocator] item and the required mod decls, as token text."""
    mod = Module(text)
    toks, items = mod.toks, []
    for i, t in enumerate(toks):
        if _is(t, 'punct', '#') and _is(_at(toks, i + 1), 'punct', '!') and _is(_at(toks, i + 2), 'punct', '['):
            items.append(' '.join(x[1] for x in toks[i:_group_end(toks, i + 2)]))
        if _is(t, 'punct', '#') and _is(_at(toks, i + 1), 'punct', '[') \
                and _is(_at(toks, i + 2), 'ident', 'global_allocator'):
            j = i
            while j < len(toks) and not _is(toks[j], 'punct', ';'):
                j += 1
            outer = [x for a in mod.attributes_before(i) for x in a]
            items.append(' '.join(repr(x[1]) if x[0] == 'str' else x[1] for x in outer + toks[i:j + 1]))
    for name, ool, attrs, _ in mod.mod_decls():
        if name in policy['freemkv_modules']:
            head = ' '.join(x[1] for a in attrs for x in a)
            items.append(f'{head} mod {name}{";" if ool else "{}"}'.strip())
    return items


# ── Workspace and fingerprint (§1) ─────────────────────────────────────────

def git(path, *args):
    return subprocess.check_output(['git', '-C', str(path), *args], text=True).strip()


def git_files(root):
    out = subprocess.check_output(['git', '-C', str(root), 'ls-files', '-z'])
    return sorted(f for f in out.decode().split('\0') if f)


def toolchain(ws):
    tc = read_toml((ws / 'freemkv' / 'Cargo.toml').read_text()).get('package', {}).get('rust-version', '')
    if not re.fullmatch(r'\d+\.\d+\.\d+', tc):
        raise ValueError(f'freemkv rust-version must be an exact X.Y.Z (got {tc!r})')
    return tc


def required_inputs(ws, policy, lock_text, features, externals):
    """R(C): everything F hashes."""
    files, roots, manifests = [], {}, {}
    for repo in list(policy['libraries']) + ['freemkv']:
        for path in git_files(ws / repo):
            kind = classify(repo, path, policy)
            data = (ws / repo / path).read_bytes() if kind else None
            if kind == 'required':
                files.append([repo, path, sha256(data)])
            elif kind == 'root':
                roots[path] = project_crate_root(data.decode(), policy)
            elif kind == 'manifest':
                manifest = read_toml(data.decode())
                if repo == 'freemkv':
                    manifests[repo] = project_freemkv_manifest(manifest, policy)
                else:
                    manifests[repo] = normalize_library_manifest(manifest)
    return {
        'files': files,
        'manifests': manifests,
        'crate_roots': roots,
        'K': closure(parse_lock(lock_text), policy),
        'features': features,
        'toolchain': toolchain(ws),
        'externals': externals,
        'policy': {'revision': policy['revision'], 'perf': policy['perf'],
                   'harness': policy['harness']},
    }


def fingerprint(inputs):
    return sha256(canonical([SCHEMA, inputs]))


def diff_inputs(old, new):
    """Human-readable reasons two R(C) differ (for the plan summary)."""
    reasons = []
    if not old:
        return ['no earlier evidence']
    of = {(r, p): h for r, p, h in old.get('files', [])}
    nf = {(r, p): h for r, p, h in new.get('files', [])}
    for key in sorted(set(of) | set(nf)):
        if of.get(key) != nf.get(key):
            reasons.append(f'{key[0]}/{key[1]} {"changed" if key in of and key in nf else "added" if key in nf else "removed"}')
    for field in ('manifests', 'crate_roots', 'K', 'features', 'toolchain', 'externals', 'policy'):
        if canonical(old.get(field)) != canonical(new.get(field)):
            reasons.append(f'{field} changed')
    return reasons


def valid_revisions(revisions):
    return (isinstance(revisions, dict) and set(revisions) == set(FIRST_PARTY)
            and all(isinstance(v, str) and SHA.fullmatch(v) for v in revisions.values()))


def patch_config(ws):
    """Point freemkv at the pinned sibling checkouts (every URL form the manifests use)."""
    config = ws / 'freemkv' / '.cargo' / 'config.toml'
    config.parent.mkdir(exist_ok=True)
    sections = ['crates-io'] + [f'"https://github.com/{OWNER}/{repo}{suffix}"'
                               for repo in SIBLINGS for suffix in ('', '.git')]
    lines = []
    for section in sections:
        lines.append(f'[patch.{section}]')
        lines += [f'{repo} = {{ path = "../{repo}" }}' for repo in SIBLINGS]
    config.write_text('\n'.join(lines) + '\n')


def resolve(ws, locked=False, run=subprocess.run):
    """cargo metadata over the patched workspace; every first-party crate must come from ws."""
    patch_config(ws)
    cmd = ['cargo', 'metadata', '--format-version', '1'] + (['--locked'] if locked else [])
    res = run(cmd, cwd=ws / 'freemkv', capture_output=True, text=True, check=True)
    found = set()
    for package in json.loads(res.stdout)['packages']:
        if package['name'] in FIRST_PARTY:
            want = (ws / package['name'] / 'Cargo.toml').resolve()
            if Path(package['manifest_path']).resolve() != want:
                raise ValueError(f'{package["name"]} resolved outside the pinned candidate snapshot')
            found.add(package['name'])
    if found != set(FIRST_PARTY):
        raise ValueError(f'first-party crates missing from the resolve: {sorted(set(FIRST_PARTY) - found)}')


def checkout(ws, revisions):
    """Fetch every sibling at its pinned sha; freemkv itself must already be at its sha."""
    if not valid_revisions(revisions):
        raise ValueError('invalid revisions')
    if git(ws / 'freemkv', 'rev-parse', 'HEAD') != revisions['freemkv']:
        raise ValueError('freemkv checkout does not match the pinned sha')
    for repo in SIBLINGS:
        path = ws / repo
        subprocess.run(['git', 'init', '-q', str(path)], check=True)
        subprocess.run(['git', '-C', str(path), 'fetch', '-q', '--depth=1',
                        f'https://github.com/{OWNER}/{repo}', revisions[repo]], check=True)
        subprocess.run(['git', '-C', str(path), 'checkout', '-q', '--detach', 'FETCH_HEAD'], check=True)
        if git(path, 'rev-parse', 'HEAD') != revisions[repo]:
            raise ValueError(f'{repo} checkout does not match {revisions[repo]}')


# ── Evidence (§1 E, I-2) ───────────────────────────────────────────────────

def gh_api(endpoint, paginate=False):
    if not paginate:
        return json.loads(subprocess.check_output(['gh', 'api', endpoint]))
    # --paginate --slurp: one JSON array of pages.
    pages = json.loads(subprocess.check_output(['gh', 'api', '--paginate', '--slurp', endpoint]))
    return [item for page in pages for item in (page if isinstance(page, list) else [page])]


def gh_download(endpoint):
    """Raw bytes of an API endpoint (an artifact zip)."""
    return subprocess.check_output(['gh', 'api', endpoint])


MAX_PLAN_ARTIFACT = 16 << 20
EXPIRY_NOTICE_DAYS = 14
# What a run's own plan fixes about the candidate it tested; the tag's evidence must agree on all of it.
PLAN_BINDING = ('schema', 'fingerprint', 'inputs', 'revisions', 'lock_sha256', 'run_id')


# GitHub REST API, "List workflow run artifacts" (`GET /repos/{owner}/{repo}/actions/runs/{run_id}/artifacts`):
#   "Lists artifacts for a workflow run."
# artifact schema: "workflow_run": {"type": "object", "nullable": true, "properties": {"id": ...,
#   "repository_id": ..., "head_repository_id": ..., "head_branch": ..., "head_sha": ...}}
# Every artifact used is checked to say it belongs to this run id at this head sha.
def plan_of_run(run_id, head_sha, request, download):
    """The evidence.json of run `run_id`'s own media-plan artifact(s). Only that run could upload
    them, so they say what it tested, whoever wrote the tag. Raises ValueError if none is readable."""
    import io
    import zipfile
    listing = request(f'repos/{OWNER}/freemkv/actions/runs/{run_id}/artifacts?name=media-plan&per_page=100')
    plans = []
    for art in listing.get('artifacts') or []:
        if art.get('name') != 'media-plan' or art.get('expired'):
            continue
        origin = art.get('workflow_run') or {}
        if origin.get('id') != run_id or origin.get('head_sha') != head_sha:
            raise ValueError(f'media-plan artifact {art.get("id")} belongs to run {origin.get("id")} at '
                             f'{str(origin.get("head_sha"))[:12]}, not run {run_id} at {str(head_sha)[:12]}')
        if not 0 < int(art.get('size_in_bytes') or 0) <= MAX_PLAN_ARTIFACT:
            raise ValueError(f'media-plan artifact {art.get("id")} has an implausible size')
        with zipfile.ZipFile(io.BytesIO(download(f'repos/{OWNER}/freemkv/actions/artifacts/{int(art["id"])}/zip'))) as z:
            info = z.getinfo('evidence.json')
            if info.file_size > MAX_EVIDENCE:
                raise ValueError('media-plan evidence.json is oversized')
            plans.append(json.loads(z.read(info)))
    if not plans:
        raise ValueError(f'run {run_id} has no unexpired media-plan artifact to bind the evidence to')
    return plans


def bind_to_plan(ev, plans):
    """The tag's evidence must be the candidate the run itself planned (F, inputs, revisions, lock)."""
    for plan in plans:
        if isinstance(plan, dict) and all(canonical(plan.get(k)) == canonical(ev.get(k)) for k in PLAN_BINDING):
            return
    raise ValueError('evidence does not match the run\'s own media-plan (F, inputs, revisions or lock differ)')


def evidence_candidates(f, request):
    """Tags for F, newest run first: [(ref, run_id)]."""
    endpoint = f'repos/{OWNER}/freemkv/git/matching-refs/tags/media-evidence/{f}/'
    refs = request(endpoint, paginate=True) if request is gh_api else request(endpoint)
    out = []
    for ref in refs:
        m = re.fullmatch(rf'refs/tags/media-evidence/{f}/(\d+)', ref.get('ref', ''))
        if m and ref.get('object', {}).get('type') == 'tag':
            out.append((ref, int(m.group(1))))
    return sorted(out, key=lambda x: -x[1])


# The identity GITHUB_TOKEN writes as (record-media-evidence names it explicitly).
ACTIONS_BOT = {'name': 'github-actions[bot]', 'email': '41898282+github-actions[bot]@users.noreply.github.com'}


def _when(stamp):
    return datetime.datetime.fromisoformat(str(stamp).replace('Z', '+00:00'))


def check_tag_provenance(tag, name, run):
    """Raise ValueError unless the tag object was written by the Actions bot under `name` while the
    run was live (created_at .. updated_at: an earlier attempt's tag stands after "Re-run all jobs").
    No ruleset is relied on: this is the consumer's own check, and trusted_run() binds the run."""
    tagger = tag.get('tagger') or {}
    if tagger.get('name') != ACTIONS_BOT['name'] or tagger.get('email') != ACTIONS_BOT['email']:
        raise ValueError(f'evidence tag was not created by {ACTIONS_BOT["name"]} '
                         f'(tagger {tagger.get("name")!r} <{tagger.get("email")}>)')
    if tag.get('tag') != name:
        raise ValueError(f'tag object is named {tag.get("tag")!r}, not {name!r}')
    try:
        when, lo, hi = _when(tagger['date']), _when(run['created_at']), _when(run['updated_at'])
    except (KeyError, TypeError, ValueError) as exc:
        raise ValueError(f'evidence tag date cannot be placed in run {run.get("id")} ({exc})') from exc
    if not lo <= when <= hi:
        raise ValueError(f'evidence tag dated {tagger["date"]} is outside run {run.get("id")} '
                         f'({lo.isoformat()} .. {hi.isoformat()})')


# GitHub REST API, "Get a reference": `GET /repos/{owner}/{repo}/git/ref/{ref}` names a ref
# unambiguously as `heads/<branch>`. By contrast, a workflow run's `head_branch` is only a name:
# the workflow-run schema is `"head_branch": {"type": "string", "nullable": true, "example":
# "master"}`, and a run on a tag reports the tag there (freemkv release.yml run 36272420399,
# event push on tag v1.7.7, has head_branch "v1.7.7"). A tag named `qa` therefore yields
# head_branch "qa" (review 2, item 1). The schema has no `ref` or `ref_type` field, so the run's
# ref cannot be read back; what is checked instead is that the qa BRANCH contains head_sha.
#
# GitHub REST API, "Compare two commits" (`GET /repos/{owner}/{repo}/compare/{basehead}`):
#   "You can compare refs (branches or tags) and commit SHAs in the same repository"
#   "This endpoint is equivalent to running the `git log BASE..HEAD` command"
#   commit-comparison schema: "status": {"type": "string",
#                                        "enum": ["diverged", "ahead", "behind", "identical"]}
# With BASE = the qa branch tip (a sha, so no branch/tag ambiguity) and HEAD = head_sha,
# "identical" or "behind" (and ahead_by == 0: `git log BASE..HEAD` is empty) means head_sha is
# the qa tip or one of its ancestors.
QA_CONTAINS = ('identical', 'behind')


def on_qa_branch(sha, request):
    """Raise ValueError unless refs/heads/qa of freemkv/freemkv contains commit `sha`."""
    tip = request(f'repos/{OWNER}/freemkv/git/ref/heads/qa')
    if tip.get('ref') != 'refs/heads/qa' or not SHA.fullmatch(str((tip.get('object') or {}).get('sha'))):
        raise ValueError('could not read refs/heads/qa')
    cmp = request(f'repos/{OWNER}/freemkv/compare/{tip["object"]["sha"]}...{sha}')
    if cmp.get('status') not in QA_CONTAINS or cmp.get('ahead_by') != 0:
        raise ValueError(f'{str(sha)[:12]} is not on the qa branch (compare status {cmp.get("status")!r}, '
                         f'ahead_by {cmp.get("ahead_by")!r}): a run on a tag named qa proves nothing')


def trusted_run(run, run_id, sha, request, finished=True):
    """Raise ValueError unless `run` is a freemkv qa.yml run whose commit is on the qa BRANCH;
    `finished` also requires it to have completed successfully (an evidence consumer; record runs
    inside it). `head_branch == 'qa'` alone is only a name: see on_qa_branch()."""
    if not (isinstance(run, dict) and run.get('id') == run_id
            and run.get('path', '').split('@')[0] == '.github/workflows/qa.yml'
            and run.get('event') in ('push', 'workflow_dispatch')
            and run.get('head_branch') == 'qa'
            and (run.get('repository') or {}).get('full_name') == f'{OWNER}/freemkv'
            and (run.get('head_repository') or {}).get('full_name') == f'{OWNER}/freemkv'
            and run.get('head_sha') == sha):
        raise ValueError('run is not a freemkv qa.yml run on the qa branch at the evidence sha')
    on_qa_branch(sha, request)
    if finished and (run.get('status') != 'completed' or run.get('conclusion') != 'success'):
        raise ValueError(f'run {run_id} is {run.get("status")}/{run.get("conclusion")}, not a successful run')


def read_evidence(ref, request):
    """(evidence.json bytes, Cargo.lock bytes, tag object) from the orphan commit an evidence tag points at."""
    tag = request(f'repos/{OWNER}/freemkv/git/tags/{ref["object"]["sha"]}')
    if tag.get('object', {}).get('type') != 'commit':
        raise ValueError('evidence tag does not point at a commit')
    commit = request(f'repos/{OWNER}/freemkv/git/commits/{tag["object"]["sha"]}')
    if commit.get('parents'):
        raise ValueError('evidence commit is not an orphan')
    tree = request(f'repos/{OWNER}/freemkv/git/trees/{commit["tree"]["sha"]}')
    blobs = {e['path']: e for e in tree.get('tree', []) if e.get('type') == 'blob'}
    if set(blobs) != {'evidence.json', 'Cargo.lock'}:
        raise ValueError(f'evidence tree holds {sorted(blobs)}')
    out = []
    for name in ('evidence.json', 'Cargo.lock'):
        if blobs[name].get('size', 0) > MAX_EVIDENCE * 4:
            raise ValueError(f'{name} is oversized')
        blob = request(f'repos/{OWNER}/freemkv/git/blobs/{blobs[name]["sha"]}')
        out.append(base64.b64decode(blob['content']))
    return out + [tag]


def required_jobs(policy):
    jobs = ['cli-matrix (linux)', 'cli-matrix (windows)', 'compare-cli-matrix', 'record-media-evidence']
    if policy['perf']['enabled']:
        jobs += ['cli-perf (linux)', 'cli-perf (windows)']
    return jobs


def legs(policy):
    return ['linux', 'windows'] + (['linux-perf', 'windows-perf'] if policy['perf']['enabled'] else [])


LEG_JOB = {'linux': 'cli-matrix (linux)', 'windows': 'cli-matrix (windows)',
           'linux-perf': 'cli-perf (linux)', 'windows-perf': 'cli-perf (windows)'}
def job_named(jobs, name):
    """The job called `name`, also when GitHub appends other matrix values ("cli-matrix (linux, …)")."""
    for job in jobs:
        n = job.get('name') or ''
        if n == name or (name.endswith(')') and n.startswith(name[:-1] + ', ')):
            return job
    return None


LEG_TARGET = {'linux': 'x86_64-unknown-linux-musl', 'windows': 'x86_64-pc-windows-msvc',
              'linux-perf': 'x86_64-unknown-linux-musl', 'windows-perf': 'x86_64-pc-windows-msvc'}


def with_record_done(jobs):
    """The run's jobs as record's self-check sees them: record-media-evidence is still running, so
    its own entry has no conclusion; replace it by the success it is about to have (job_named
    returns the first match, so the running entry must not stay)."""
    return [j for j in jobs if j.get('name') != 'record-media-evidence'] + [
        {'name': 'record-media-evidence', 'conclusion': 'success'}]


def check_evidence(f, run_id, evidence_bytes, lock_bytes, run, jobs, policy, request, perf_check=None,
                   finished=True):
    """Raise ValueError unless this evidence proves F (I-2). Returns warnings. `finished=False` is
    record's self-check from inside the still-running run."""
    if len(evidence_bytes) > MAX_EVIDENCE:
        raise ValueError('evidence.json is oversized')
    ev = json.loads(evidence_bytes)
    if not isinstance(ev, dict) or ev.get('schema') != SCHEMA:
        raise ValueError(f'schema {ev.get("schema") if isinstance(ev, dict) else "?"} != {SCHEMA}')
    inputs = ev.get('inputs')
    if fingerprint(inputs) != f or ev.get('fingerprint') != f:
        raise ValueError('F does not recompute from the evidence')
    if ev.get('lock_sha256') != sha256(lock_bytes):
        raise ValueError('lock digest mismatch')
    if closure(parse_lock(lock_bytes.decode()), policy) != inputs.get('K'):
        raise ValueError('K does not recompute from the stored lock')
    revisions = ev.get('revisions')
    if not valid_revisions(revisions) or ev.get('run_id') != run_id:
        raise ValueError('revisions or run id malformed')
    trusted_run(run, run_id, revisions['freemkv'], request, finished=finished)
    for name in required_jobs(policy):
        if (job_named(jobs, name) or {}).get('conclusion') != 'success':
            raise ValueError(f'job {name!r} is not success')
    runner_re = re.compile(policy['runner_name_re'])
    label = f'run-{run_id}'
    for leg in legs(policy):
        rec = (ev.get('legs') or {}).get(leg)
        job = job_named(jobs, LEG_JOB[leg])
        if not isinstance(rec, dict):
            raise ValueError(f'leg {leg} not recorded')
        m = runner_re.fullmatch(job.get('runner_name') or '')
        if not m:
            raise ValueError(f'leg {leg} ran on {job.get("runner_name")!r}')
        if rec.get('instance_id') != m.group(3):
            raise ValueError(f'leg {leg} instance {rec.get("instance_id")!r} is not the runner {job["runner_name"]!r}')
        if rec.get('runner_name') != job.get('runner_name'):
            raise ValueError(f'leg {leg} record names {rec.get("runner_name")!r}, the job ran on {job["runner_name"]!r}')
        if bool(m.group(2)) != leg.endswith('-perf'):
            raise ValueError(f'leg {leg} ran on the wrong runner class')
        if label not in (job.get('labels') or []):
            raise ValueError(f'leg {leg} lacked the {label} label')
        if rec.get('launched_by') != run_id:
            raise ValueError(f'leg {leg} instance not confirmed launched by this run')
        if rec.get('rustc_release') != inputs.get('toolchain'):
            raise ValueError(f'leg {leg} rustc {rec.get("rustc_release")!r} != TC')
        if rec.get('target') != LEG_TARGET[leg]:
            raise ValueError(f'leg {leg} built {rec.get("target")!r}')
        if not rec.get('instance_type') or not rec.get('c_toolchain') or rec.get('env_clean') is not True:
            raise ValueError(f'leg {leg} lacks instance type, C toolchain or a clean environment')
    if policy['perf']['enabled']:
        perf = ev.get('perf')
        if not isinstance(perf, dict) or set(perf) != {'linux', 'windows'}:
            raise ValueError('perf results missing')
        if perf_check is None:
            raise ValueError('no perf verdict checker')
        for os_name, result in perf.items():
            if not perf_check(result, policy):
                raise ValueError(f'perf verdict for {os_name} does not recompute')
    # Evidence EXPIRES after policy max_age_days (90): that is the media-plan artifact's
    # retention-days in qa.yml, and without the artifact the evidence cannot be bound to its run
    # (bind_to_plan), so it stops counting then. Expiry is stated here, not left to a 404.
    warnings = []
    try:
        age = (datetime.datetime.now(datetime.timezone.utc) - _when(run['created_at'])).days
    except (KeyError, TypeError, ValueError) as exc:
        raise ValueError(f'run {run_id} has no readable created_at ({exc})') from exc
    limit = policy['max_age_days']
    if age >= limit:
        raise ValueError(f'evidence expired: it is {age} days old and evidence counts for {limit} days '
                         '(the media-plan retention that binds it to its run); qa runs the full disc again')
    if age >= limit - EXPIRY_NOTICE_DAYS:
        warnings.append(f'evidence is {age} days old and expires at {limit} days (in {limit - age}); '
                        'after that qa runs the full disc again')
    return ev, warnings


def find_evidence(f, policy, request=gh_api, perf_check=None, log=print, download=gh_download):
    """Newest valid evidence for F, or None. Any error means 'not proven' (fail-safe run).
    The tag's own fields are forgeable by anyone who can push a tag; what binds F to a real run is
    that run's media-plan artifact, which only the run itself can upload (retention: 90 days)."""
    try:
        candidates = evidence_candidates(f, request)
    except Exception as exc:  # noqa: BLE001 — fail-safe: an unreadable lookup forces a run
        log(f'::warning::evidence lookup failed ({exc}); running')
        return None
    for ref, run_id in candidates:
        try:
            evidence_bytes, lock_bytes, tag = read_evidence(ref, request)
            run = request(f'repos/{OWNER}/freemkv/actions/runs/{run_id}')
            jobs = request(f'repos/{OWNER}/freemkv/actions/runs/{run_id}/jobs?filter=latest&per_page=100')['jobs']
            # Who wrote the tag, and for which real run: a tag anyone else pushed is not evidence.
            check_tag_provenance(tag, f'media-evidence/{f}/{run_id}', run)
            ev, warnings = check_evidence(f, run_id, evidence_bytes, lock_bytes, run, jobs, policy, request, perf_check)
            bind_to_plan(ev, plan_of_run(run_id, run['head_sha'], request, download))
            return {'run_id': run_id, 'evidence': ev, 'warnings': warnings,
                    'url': f'https://github.com/{OWNER}/freemkv/actions/runs/{run_id}'}
        except Exception as exc:  # noqa: BLE001 — one bad tag never blocks a newer or older good one
            log(f'::notice::{ref.get("ref")}: not valid evidence ({exc})')
    return None


def decide(proven, run_media=False, skip=False, skip_reason='', canary_ok=True, superseded=False, canary_why=''):
    """(status, run, reason). Red statuses: waived, superseded, canary-failed."""
    if superseded:
        return 'superseded', False, 'a newer candidate is on this branch; this run proves nothing'
    if not canary_ok:
        return 'canary-failed', False, ('key service broken: the canary did not return the expected keys'
                                        + (f' ({canary_why})' if canary_why else ''))
    if run_media:
        return 'run', True, 'run_media requested'
    if proven:
        return 'reuse', False, 'full-disc not needed: every rip-affecting input matches green evidence'
    if skip:
        if not skip_reason.strip():
            raise ValueError('a dispatch skip_media needs a skip_reason')
        return 'waived', False, f'full-disc required but skipped ({skip_reason.strip()}): unproven'
    return 'run', True, 'rip-affecting inputs lack green evidence'


def canary(policy, result, ref):
    """D2 / decision 15: (ok, why, notice) from tests/media_canary.py's result for this run.
    It runs where the gate runs (qa); elsewhere it is not asked. On qa, a missing, malformed or
    failed result, or one that skipped a configured probe, fails closed."""
    probes = [p['fixture'] for p in policy['canary'].get('probes', [])]
    if not probes:
        return True, '', None
    if ref != 'refs/heads/qa':
        return True, '', f'the key-service canary runs on the qa branch only (this run is on {ref})'
    if not isinstance(result, dict):
        return False, 'the canary did not run or wrote no result', None
    got = {p.get('fixture'): p for p in result.get('probes') or [] if isinstance(p, dict)}
    bad = [f'{f}: {(got.get(f) or {}).get("reason") or "not probed"}' for f in probes
           if (got.get(f) or {}).get('ok') is not True]
    if bad or result.get('ok') is not True:
        return False, '; '.join(bad) or result.get('error') or 'the canary failed', None
    return True, '', None


# ── Plan artifact: seal and restore ────────────────────────────────────────

def seal(plan_dir, run_id, sha):
    """The plan must belong to this run and candidate; any attempt of the run may use it."""
    ev = json.loads((plan_dir / 'evidence.json').read_text())
    if ev.get('run_id') != int(run_id) or (ev.get('revisions') or {}).get('freemkv') != sha:
        raise ValueError('plan belongs to another run or candidate')
    lock = (plan_dir / 'Cargo.lock').read_bytes()
    if ev.get('lock_sha256') != sha256(lock):
        raise ValueError('plan lock differs from the plan')
    return ev


def restore(ws, plan_dir, run=subprocess.run):
    ev = json.loads((plan_dir / 'evidence.json').read_text())
    if not valid_revisions(ev.get('revisions')):
        raise ValueError('invalid plan revisions')
    for repo, sha in ev['revisions'].items():
        if git(ws / repo, 'rev-parse', 'HEAD') != sha:
            raise ValueError(f'{repo} checkout differs from the plan')
    lock = (plan_dir / 'Cargo.lock').read_bytes()
    if sha256(lock) != ev.get('lock_sha256'):
        raise ValueError('resolved dependency lock differs from the plan')
    (ws / 'freemkv' / 'Cargo.lock').write_bytes(lock)
    resolve(ws, locked=True, run=run)


def lock_assert(base_text, candidate_text, k_only=False, policy=None):
    """Third-party entries (or K) of two locks must be equal, both normalized. Returns differences."""
    if k_only:
        a, b = closure(parse_lock(base_text), policy), closure(parse_lock(candidate_text), policy)
    else:
        a, b = third_party(parse_lock(base_text)), third_party(parse_lock(candidate_text))
    sa, sb = {canonical(x) for x in a}, {canonical(x) for x in b}
    return sorted(x.decode() for x in sa ^ sb)


# ── Plan (§3.2 job 1) ──────────────────────────────────────────────────────

def plan(ws, policy, env, externals, request=gh_api, run=subprocess.run, canary_result=None, perf_check=None,
         tree=resolved_features, download=gh_download):
    """Pin, resolve, guard, fingerprint, look up, decide. Returns (outputs, evidence dict, lock text)."""
    revisions = json.loads(env['REVISIONS'])
    if not valid_revisions(revisions):
        raise ValueError('could not pin C')
    committed = (ws / 'freemkv' / 'Cargo.lock').read_text()
    resolve(ws, run=run)
    lock_text = (ws / 'freemkv' / 'Cargo.lock').read_text()
    warnings = []
    drift = lock_assert(committed, lock_text)
    if drift:
        warnings.append(f'the committed Cargo.lock is stale against C: {len(drift)} third-party entries '
                        'differ; F uses the resolved L_qa')
    errors = run_guards(ws, policy, lock_text)
    if errors:
        raise GuardError('\n'.join(errors))
    k_names = [e[0] for e in closure(parse_lock(lock_text), policy)]
    inputs = required_inputs(ws, policy, lock_text, tree(ws / 'freemkv', k_names, run=run) if tree else {},
                             externals)
    f = fingerprint(inputs)
    notices = []
    if not policy['canary'].get('probes'):
        warnings.append('the key-service canary has no probes configured (policy canary.probes); '
                        'decision 15 is not enforced until one is added')
    canary_ok, canary_why, note = canary(policy, canary_result, env.get('GITHUB_REF', ''))
    if note:
        notices.append(note)
    proven = find_evidence(f, policy, request, perf_check, download=download) if canary_ok else None
    status, go, reason = decide(proven is not None, env.get('RUN_MEDIA') == 'true',
                                env.get('SKIP_MEDIA') == 'true' or '[skip-media]' in env.get('HEAD_MESSAGE', ''),
                                env.get('SKIP_REASON') or ('[skip-media]' if '[skip-media]' in env.get('HEAD_MESSAGE', '') else ''),
                                canary_ok, env.get('SUPERSEDED') == 'true', canary_why)
    if proven:
        warnings += proven['warnings']
    evidence = {'schema': SCHEMA, 'fingerprint': f, 'inputs': inputs, 'revisions': revisions,
                'lock_sha256': sha256(lock_text.encode()), 'run_id': int(env['GITHUB_RUN_ID'])}
    outputs = {'status': status, 'run': str(go).lower(), 'fingerprint': f, 'reason': reason,
               'revisions': json.dumps(revisions, sort_keys=True), 'toolchain': inputs['toolchain'],
               'evidence_url': proven['url'] if proven else '', 'harness': policy['harness']['sha'],
               'legs': json.dumps(legs(policy)),
               'launch_templates': json.dumps({k: v for k, v in externals.get('launch_templates', {}).items()
                                               if isinstance(v, dict)}, sort_keys=True),
               'warnings': warnings, 'notices': notices}
    return outputs, evidence, lock_text


# ── qa.yml wiring (§3.2): pin, externals, leg identity, record, verdict ────

BRANCH_REFS = {'refs/heads/qa': 'qa', 'refs/heads/dev': 'dev'}


def pin(ref, sha, requested, request=gh_api):
    """C for this run: the dispatched revisions (each on the branch) or the sibling branch tips.
    `ref` is the full GITHUB_REF: a workflow dispatched on a TAG named qa has GITHUB_REF_NAME "qa"
    too, and is refused here.

    GitHub REST API, "Get a commit" (`GET /repos/{owner}/{repo}/commits/{ref}`), parameter ref:
    "The commit reference. Can be a commit SHA, branch name (`heads/BRANCH_NAME`), or tag name
    (`tags/TAG_NAME`)." So sibling tips are read as `heads/<branch>`, never a bare name that a tag
    could shadow, and every comparison is between shas."""
    if ref not in BRANCH_REFS:
        raise ValueError(f'the media gate runs on the qa and dev branches only, not {ref!r}')
    branch = BRANCH_REFS[ref]
    tips = {}
    for repo in SIBLINGS:
        tips[repo] = request(f'repos/{OWNER}/{repo}/commits/heads/{branch}')['sha']
    if requested:
        revisions = json.loads(requested)
        if not valid_revisions(revisions) or revisions['freemkv'] != sha:
            raise ValueError('dispatched revisions are malformed or not this freemkv sha')
        for repo in SIBLINGS:
            status = request(f'repos/{OWNER}/{repo}/compare/{revisions[repo]}...{tips[repo]}')['status']
            if status not in ('ahead', 'identical'):
                raise ValueError(f'{repo} {revisions[repo][:12]} is not on {branch} ({status})')
    else:
        revisions = dict(tips, freemkv=sha)
        if not valid_revisions(revisions):
            raise ValueError('could not read every sibling tip')
    superseded = False
    if branch == 'qa':
        superseded = request(f'repos/{OWNER}/freemkv/git/ref/heads/qa')['object']['sha'] != sha
    return revisions, superseded


def launch_templates(policy):
    """The functional templates. Perf legs launch from these too, with run-instances overrides."""
    return sorted({cfg['template'] for cfg in policy['launch'].values()})


def leg_labels(leg, run_id):
    """The runner labels a leg's instance registers with (freemkv-media[-perf],<os>,run-<id>)."""
    os_name, perf = leg.split('-')[0], leg.endswith('-perf')
    return f'freemkv-media{"-perf" if perf else ""},{os_name},run-{run_id}'


# The launch-template fields an On-Demand launch reproduces without the template, and the ones it
# deliberately replaces. Anything else in the pinned version fails the launch rather than silently
# launching something the template would not have.
ON_DEMAND_CARRIED = ('ImageId', 'InstanceType', 'IamInstanceProfile', 'SecurityGroupIds', 'BlockDeviceMappings',
                     'MetadataOptions', 'InstanceInitiatedShutdownBehavior', 'TagSpecifications')
ON_DEMAND_REPLACED = ('InstanceMarketOptions', 'UserData')   # On-Demand, and this commit's user-data
OWN_TAGS = ('freemkv-ci', 'runner-token-param', 'runner-repo', 'launched-by', 'runner-labels')


def on_demand_launch(data, template, version, mappings):
    """run-instances input for an On-Demand instance equivalent to `template` v`version`, launched
    WITHOUT the template: run-instances can override a template's InstanceMarketOptions but never
    clear them, so an On-Demand launch through a Spot template would still be Spot.
    Returns {'input': <run-instances JSON>, 'tags': [template instance tags not set by the launcher]}."""
    unknown = sorted(set(data) - set(ON_DEMAND_CARRIED) - set(ON_DEMAND_REPLACED))
    if unknown:
        raise ValueError(f'{template} v{version} sets {unknown}, which an On-Demand launch without the template '
                         'does not reproduce; add them to ON_DEMAND_CARRIED or drop them from the template')
    for field in ('ImageId', 'IamInstanceProfile', 'SecurityGroupIds', 'BlockDeviceMappings', 'MetadataOptions'):
        if not data.get(field):
            raise ValueError(f'{template} v{version} has no {field} for an On-Demand launch')
    if (data['MetadataOptions'] or {}).get('InstanceMetadataTags') != 'enabled':
        raise ValueError(f'{template} v{version} does not enable InstanceMetadataTags: the user-data reads its '
                         'token parameter and labels from the instance tags')
    if data.get('InstanceInitiatedShutdownBehavior') != 'terminate':
        raise ValueError(f'{template} v{version}: InstanceInitiatedShutdownBehavior must be terminate '
                         '(the runner deletes itself by shutting down)')
    out = {k: data[k] for k in ('ImageId', 'InstanceType', 'IamInstanceProfile', 'SecurityGroupIds',
                                'MetadataOptions', 'InstanceInitiatedShutdownBehavior') if k in data}
    out['BlockDeviceMappings'] = mappings if mappings is not None else data['BlockDeviceMappings']
    others = sorted({str(spec.get('ResourceType')) for spec in data.get('TagSpecifications') or []
                     if spec.get('ResourceType') != 'instance'})
    if others:
        raise ValueError(f'{template} v{version} tags {others} at launch, which an On-Demand launch without the '
                         'template does not reproduce (only instance tags are carried); drop them from the template '
                         'or teach on_demand_launch to pass them')
    tags = [t for spec in data.get('TagSpecifications') or [] if spec.get('ResourceType') == 'instance'
            for t in spec.get('Tags') or [] if t.get('Key') not in OWN_TAGS]
    return {'input': out, 'tags': tags}


def launch_spec(policy, leg, run_id, ref, templates, aws=None, attempt=1):
    """Everything the launch step passes to run-instances for one leg, from the policy alone:
    - the functional template at its planned version (no separate perf templates exist);
    - perf differences as overrides: the perf instance type, no type fallback, the root volume;
    - the market: Spot through the template with the policy's price cap (never the template's);
      then, if the policy asks, On-Demand WITHOUT the template (on_demand_launch), from the pinned
      version's own image, profile, security groups, volumes and metadata options;
    - a registration-token parameter per run, attempt and leg (the roles' existing
      /freemkv-ci/runner-reg/* scope); the instance deletes it after reading.
    qa branch only: evidence is recorded only for qa-branch runs, so any other launch proves nothing."""
    if ref != 'refs/heads/qa':
        raise ValueError(f'the media legs launch on the qa branch only, not {ref!r} '
                         '(evidence is recorded for qa-branch runs only)')
    if leg not in legs(policy):
        raise ValueError(f'leg {leg!r} is not launched under this policy ({legs(policy)})')
    os_name, perf = leg.split('-')[0], leg.endswith('-perf')
    cfg = policy['launch'][os_name]
    template = cfg['template']
    # The version plan-media pinned (and fingerprinted into F). Without it there is nothing to
    # launch: '$Default' would be read at launch time and could be a version F never saw.
    pin = templates.get(template)
    if not isinstance(pin, dict) or not isinstance(pin.get('version'), int) or isinstance(pin.get('version'), bool) \
            or pin['version'] < 1:
        raise ValueError(f'{template} has no pinned version in the plan ({pin!r}); refusing to launch '
                         'an unpinned template (the plan could not read the launch templates)')
    version = str(pin['version'])
    types = [policy['perf']['instance_type']] if perf else list(cfg.get('types', []))
    markets = [{'name': 'template', 'options': None, 'template': True}]
    if cfg.get('spot_max_price'):
        markets = [{'name': 'spot', 'template': True, 'options': {'MarketType': 'spot', 'SpotOptions': {
            'MaxPrice': str(cfg['spot_max_price']), 'SpotInstanceType': 'one-time',
            'InstanceInterruptionBehavior': 'terminate'}}}]
        if cfg.get('on_demand_fallback'):
            markets.append({'name': 'on-demand', 'options': None, 'template': False})
    need_mappings = perf and policy['perf'].get('root_volume_gib')
    on_demand = any(not m['template'] for m in markets)
    data = {}
    if need_mappings or on_demand:
        data = (aws or aws_json)('ec2', 'describe-launch-template-versions', '--launch-template-name', template,
                                 '--versions', version)['LaunchTemplateVersions'][0]['LaunchTemplateData']
    mappings = None
    if need_mappings:
        mappings = [dict(m, Ebs=dict(m['Ebs'])) if 'Ebs' in m else dict(m)
                    for m in data.get('BlockDeviceMappings') or []]
        root = next((m for m in mappings if 'Ebs' in m), None)
        if root is None:
            raise ValueError(f'{template} v{version} has no EBS root volume to resize for {leg}')
        root['Ebs']['VolumeSize'] = int(policy['perf']['root_volume_gib'])
    od = on_demand_launch(data, template, version, mappings) if on_demand else None
    return {'leg': leg, 'os': os_name, 'template': template, 'version': version, 'types': types,
            'markets': markets, 'block_device_mappings': mappings, 'on_demand': od, 'labels': leg_labels(leg, run_id),
            'job': LEG_JOB[leg], 'param': f'/freemkv-ci/runner-reg/{int(run_id)}-{int(attempt)}-{leg}',
            'user_data': f'.github/runner-templates/user-data-{os_name}.{"ps1" if os_name == "windows" else "sh"}'}


def aws_json(*args, run=subprocess.run):
    res = run(['aws', *args, '--output', 'json'], capture_output=True, text=True, check=True)
    return json.loads(res.stdout)


def read_externals(part, policy, bucket=None, run=subprocess.run):
    """The fixture pins (S3 HEAD) or the launch-template pins (the $Default version)."""
    if part == 'fixtures':
        out = {}
        for key in policy['fixtures']:
            head = aws_json('s3api', 'head-object', '--bucket', bucket, '--key', key, run=run)
            version = head.get('VersionId')
            pin = {'etag': head['ETag'], 'size': head['ContentLength'],
                   'version_id': version if version not in (None, 'null') else None}
            if '-' in head['ETag'].strip('"'):
                # The exact upload part size, so tests/media_fetch.py can verify the ETag in flight.
                first = aws_json('s3api', 'head-object', '--bucket', bucket, '--key', key, '--part-number', '1',
                                 '--if-match', head['ETag'], run=run)
                pin.update(part_size=first['ContentLength'], parts=first.get('PartsCount'))
            out[key] = pin
        return out
    out = {}
    for name in launch_templates(policy):
        data = aws_json('ec2', 'describe-launch-template-versions', '--launch-template-name', name,
                        '--versions', '$Default', run=run)['LaunchTemplateVersions'][0]
        lt = data['LaunchTemplateData']
        out[name] = {'version': data['VersionNumber'], 'image_id': lt.get('ImageId'),
                     'instance_type': lt.get('InstanceType')}
    return out


def imds(path, opener=None):
    import urllib.request
    opener = opener or urllib.request.urlopen
    token = opener(urllib.request.Request('http://169.254.169.254/latest/api/token', method='PUT', headers={
        'X-aws-ec2-metadata-token-ttl-seconds': '60'}), timeout=5).read().decode()
    return opener(urllib.request.Request(f'http://169.254.169.254/latest/meta-data/{path}', headers={
        'X-aws-ec2-metadata-token': token}), timeout=5).read().decode().strip()


def leg_identity(leg, target, c_toolchain, env, run=subprocess.run, meta=imds):
    """What an EC2 leg records about itself; record-media-evidence cross-checks every field."""
    rustc = run(['rustc', '-Vv'], capture_output=True, text=True, check=True).stdout
    release = next((l.split(':', 1)[1].strip() for l in rustc.splitlines() if l.startswith('release:')), '')
    return {'leg': leg, 'runner_name': env.get('RUNNER_NAME', ''), 'target': target, 'rustc_release': release,
            'rustc': rustc.strip(), 'instance_id': meta('instance-id'), 'instance_type': meta('instance-type'),
            'c_toolchain': c_toolchain.strip(), 'env_clean': True}


def gh_post(endpoint, body):
    res = subprocess.run(['gh', 'api', '-X', 'POST', endpoint, '--input', '-'], input=json.dumps(body),
                         capture_output=True, text=True, check=True)
    return json.loads(res.stdout)


def write_evidence_tag(f, run_id, evidence_bytes, lock_bytes, post=gh_post, request=gh_api, now=None):
    """The orphan commit holding evidence.json and Cargo.lock, and its annotated tag (decision 6).
    The tagger is the Actions bot (what check_tag_provenance requires). A tag already at the name
    counts only if it is this run's own evidence for F from an earlier attempt."""
    base = f'repos/{OWNER}/freemkv/git'
    blobs = [post(f'{base}/blobs', {'content': base64.b64encode(data).decode(), 'encoding': 'base64'})['sha']
             for data in (evidence_bytes, lock_bytes)]
    tree = post(f'{base}/trees', {'tree': [
        {'path': 'evidence.json', 'mode': '100644', 'type': 'blob', 'sha': blobs[0]},
        {'path': 'Cargo.lock', 'mode': '100644', 'type': 'blob', 'sha': blobs[1]}]})['sha']
    commit = post(f'{base}/commits', {'message': f'media evidence {f} (run {run_id})', 'tree': tree,
                                      'parents': []})['sha']
    name = f'media-evidence/{f}/{run_id}'
    stamp = (now or datetime.datetime.now(datetime.timezone.utc)).strftime('%Y-%m-%dT%H:%M:%SZ')
    tag = post(f'{base}/tags', {'tag': name, 'message': f'full-disc evidence for {f} from run {run_id}',
                                'object': commit, 'type': 'commit', 'tagger': dict(ACTIONS_BOT, date=stamp)})['sha']
    try:
        post(f'{base}/refs', {'ref': f'refs/tags/{name}', 'sha': tag})
    except subprocess.CalledProcessError as exc:
        if 'Reference already exists' not in (exc.stdout or '') + (exc.stderr or ''):
            raise
        # "Re-run all jobs" of a run that already recorded: its evidence tag stands, but only if it
        # really is ours. A tag someone else pushed at this name is refused, not adopted.
        existing = request(f'{base}/ref/tags/{name}')
        old_ev, _, old_tag = read_evidence(existing, request)
        tagger = old_tag.get('tagger') or {}
        old = json.loads(old_ev)
        if (existing.get('object', {}).get('type') != 'tag' or old_tag.get('tag') != name
                or tagger.get('name') != ACTIONS_BOT['name'] or tagger.get('email') != ACTIONS_BOT['email']
                or old.get('fingerprint') != f or old.get('run_id') != run_id):
            raise ValueError(f'refs/tags/{name} already exists and is not this run\'s evidence '
                             f'(tagger {tagger.get("name")!r}); refusing to adopt it')
        print(f'::notice::refs/tags/{name} already exists (an earlier attempt of this run recorded it)')
        commit = old_tag['object']['sha']
    return name, commit


def record(plan_dir, legs_dir, policy, env, request=gh_api, aws=aws_json, post=gh_post):
    """Seal the plan, cross-check every leg against the jobs API and EC2, then write the evidence tag."""
    run_id = int(env['GITHUB_RUN_ID'])
    ev = seal(plan_dir, run_id, env['GITHUB_SHA'])
    lock = (plan_dir / 'Cargo.lock').read_bytes()
    run = request(f'repos/{OWNER}/freemkv/actions/runs/{run_id}')
    jobs = request(f'repos/{OWNER}/freemkv/actions/runs/{run_id}/jobs?filter=latest&per_page=100')['jobs']
    runner_re = re.compile(policy['runner_name_re'])
    # What each launch job wrote when it started an instance (any attempt of this run).
    launched = {}
    for path in legs_dir.glob('launch-*.json'):
        att = json.loads(path.read_text())
        launched[att.get('instance_id')] = att
    legs_out = {}
    for leg in legs(policy):
        rec = json.loads((legs_dir / f'leg-{leg}.json').read_text())
        job = job_named(jobs, LEG_JOB[leg]) or {}
        m = runner_re.fullmatch(job.get('runner_name') or '')
        if not m or rec.get('runner_name') != job.get('runner_name') or rec.get('instance_id') != m.group(3):
            raise ValueError(f'leg {leg}: runner {job.get("runner_name")!r} does not match its record')
        os_name = leg.split('-')[0]
        want_labels = leg_labels(leg, run_id)
        att = launched.get(rec['instance_id'])
        if not att or att.get('launched_by') != run_id or att.get('os') != os_name \
                or att.get('runner_labels') != want_labels:
            raise ValueError(f'leg {leg}: instance {rec["instance_id"]} has no launch record from run {run_id}')
        # EC2 keeps a terminated instance visible for about an hour; confirm while it can.
        try:
            reservations = aws('ec2', 'describe-instances', '--instance-ids', rec['instance_id'])['Reservations']
        except subprocess.CalledProcessError as exc:
            # Only "gone after termination" falls back to the launch record; any other error is fatal.
            if 'InvalidInstanceID.NotFound' not in f'{exc.stdout or ""}{exc.stderr or ""}{exc.output or ""}':
                raise
            print(f'::notice::{leg}: {rec["instance_id"]} no longer visible in EC2; using its launch record')
            reservations = []
        for res in reservations:
            for inst in res['Instances']:
                tags = {t['Key']: t['Value'] for t in inst.get('Tags', [])}
                if tags.get('launched-by') != str(run_id) or tags.get('runner-labels') != want_labels:
                    raise ValueError(f'leg {leg}: instance {rec["instance_id"]} was not launched by run {run_id}')
        rec['launched_by'] = run_id
        rec['launch_template'] = att.get('launch_template')
        rec['launch_template_version'] = att.get('launch_template_version')
        legs_out[leg] = rec
    ev['legs'] = legs_out
    evidence_bytes = json.dumps(ev, indent=1, sort_keys=True).encode()
    done = with_record_done(jobs)
    check_evidence(ev['fingerprint'], run_id, evidence_bytes, lock, run, done, policy, request, finished=False)
    return write_evidence_tag(ev['fingerprint'], run_id, evidence_bytes, lock, post, request)


def verdict(env):
    """I-1: (green, lines). The one media answer for this qa run."""
    status, reason = env.get('STATUS', ''), env.get('REASON', '')
    lines = [f'### Full-disc media: {status or "no plan"}', '', reason]
    if status == 'canary-failed':
        return False, lines + ['', 'the key-service canary failed, so no EC2 ran and qa is red (decision 15). '
                                   '"Re-run failed jobs" re-runs the canary.']
    if env.get('PLAN_RESULT') != 'success':
        return False, lines + ['', 'plan-media failed: the candidate could not be pinned or a classification '
                                   'guard fired (see its log). qa is red.']
    # The GUI gate (tests/gui_gate.py) runs on every candidate, reused media evidence or not.
    gui = env.get('GUI', '')
    if gui != 'success':
        return False, lines + ['', f'the GUI gate is {gui or "not run"}: qa is red']
    if status == 'reuse':
        return True, lines + ['', f'full-disc not needed: evidence {env.get("EVIDENCE_URL", "")}']
    if status == 'run':
        results = {k: env.get(k, '') for k in ('MATRIX', 'COMPARE', 'RECORD')}
        ok = all(v == 'success' for v in results.values())
        lines += ['', ', '.join(f'{k.lower()}={v or "not run"}' for k, v in results.items())]
        return ok, lines + ([f'evidence tag: {env.get("TAG", "")}'] if ok else ['the full-disc run is not green: qa is red'])
    return False, lines + ['', f'{status}: qa is red (unproven)']


# ── CLI ────────────────────────────────────────────────────────────────────

def _write_outputs(outputs):
    """GITHUB_OUTPUT lines. Values are single-line (newlines folded) and written with a random
    delimiter, so text such as a skip reason can never inject a second output."""
    path = os.environ.get('GITHUB_OUTPUT')
    chunks = []
    for key, value in outputs.items():
        if key in ('warnings', 'notices'):
            continue
        value = ' '.join(str(value).splitlines())
        delim = 'EOF_' + os.urandom(8).hex()
        chunks.append(f'{key}<<{delim}\n{value}\n{delim}\n')
        print(f'{key}={value}')
    if path:
        with open(path, 'a') as out:
            out.write(''.join(chunks))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    parser.add_argument('mode', choices=('guards', 'checkout', 'plan', 'restore', 'seal',
                                         'release-lock-assert', 'fingerprint', 'pin', 'externals',
                                         'leg', 'record', 'verdict', 'launch-spec'))
    parser.add_argument('--workspace', type=Path, default=Path.cwd())
    parser.add_argument('--plan-directory', type=Path, default=Path('media-plan'))
    parser.add_argument('--revisions')
    parser.add_argument('--externals', type=Path)
    parser.add_argument('--base', type=Path)
    parser.add_argument('--candidate', type=Path)
    parser.add_argument('--k-only', action='store_true')
    parser.add_argument('--policy', type=Path, default=POLICY)
    parser.add_argument('--part', choices=('fixtures', 'launch_templates'))
    parser.add_argument('--bucket')
    parser.add_argument('--out', type=Path)
    parser.add_argument('--leg')
    parser.add_argument('--target')
    parser.add_argument('--c-toolchain', default='')
    parser.add_argument('--legs', type=Path)
    parser.add_argument('--canary', type=Path, help="tests/media_canary.py's result (plan)")
    args = parser.parse_args(argv)
    policy = load_policy(args.policy)
    ws = args.workspace
    try:
        if args.mode == 'guards':
            errors = run_guards(ws, policy, (ws / 'freemkv' / 'Cargo.lock').read_text())
            for e in errors:
                print(f'::error::{e}')
            return 1 if errors else 0
        if args.mode == 'checkout':
            checkout(ws, json.loads(args.revisions))
            patch_config(ws)
            return 0
        if args.mode == 'restore':
            restore(ws, args.plan_directory)
            return 0
        if args.mode == 'seal':
            seal(args.plan_directory, os.environ['GITHUB_RUN_ID'], os.environ['GITHUB_SHA'])
            return 0
        if args.mode == 'pin':
            revisions, superseded = pin(os.environ['GITHUB_REF'], os.environ['GITHUB_SHA'],
                                        os.environ.get('REQUESTED', ''))
            checkout(ws, revisions)
            _write_outputs({'revisions': json.dumps(revisions, sort_keys=True),
                            'superseded': str(superseded).lower()})
            return 0
        if args.mode == 'externals':
            current = json.loads(args.out.read_text()) if args.out.exists() else {}
            try:
                current[args.part] = read_externals(args.part, policy, args.bucket)
            except Exception as exc:  # noqa: BLE001 — fail-safe: unreadable externals can only force a run
                print(f'::warning::could not read the {args.part} pins ({exc}); the plan will run, not reuse')
                current[args.part] = {'error': str(exc)[:500]}
            args.out.write_text(json.dumps(current, indent=1, sort_keys=True) + '\n')
            return 0
        if args.mode == 'leg':
            rec = leg_identity(args.leg, args.target, args.c_toolchain, os.environ)
            args.out.write_text(json.dumps(rec, indent=1, sort_keys=True) + '\n')
            print(json.dumps(rec, indent=1, sort_keys=True))
            return 0
        if args.mode == 'launch-spec':
            spec = launch_spec(policy, args.leg, int(os.environ['GITHUB_RUN_ID']), os.environ['GITHUB_REF'],
                               json.loads(os.environ.get('TEMPLATES') or '{}'),
                               attempt=int(os.environ.get('GITHUB_RUN_ATTEMPT') or 1))
            print(json.dumps(spec, sort_keys=True))
            return 0
        if args.mode == 'record':
            name, commit = record(args.plan_directory, args.legs, policy, os.environ)
            print(f'wrote refs/tags/{name} -> {commit}')
            _write_outputs({'tag': name})
            return 0
        if args.mode == 'verdict':
            green, lines = verdict(os.environ)
            # A reason may carry dispatch text; never let a line start a workflow command.
            text = '\n'.join(line.replace('::', ': :') for line in lines) + '\n'
            print(text)
            if os.environ.get('GITHUB_STEP_SUMMARY'):
                with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as out:
                    out.write(text)
            if not green:
                print(f'::error title=Media verdict::{lines[0].lstrip("# ")} — see the summary')
            return 0 if green else 1
        if args.mode == 'release-lock-assert':
            diff = lock_assert(args.base.read_text(), args.candidate.read_text(), args.k_only, policy)
            for d in diff:
                print(f'::error::third-party lock entry differs: {d}')
            return 1 if diff else 0
        if args.mode == 'plan' and not args.externals:
            raise ValueError('plan needs --externals (the fixture and launch-template pins)')
        externals = json.loads(args.externals.read_text()) if args.externals else {}
        if args.mode == 'fingerprint':
            lock = (ws / 'freemkv' / 'Cargo.lock').read_text()
            k = [e[0] for e in closure(parse_lock(lock), policy)]
            print(fingerprint(required_inputs(ws, policy, lock, resolved_features(ws / 'freemkv', k), externals)))
            return 0
        canary_result = None
        if args.canary and args.canary.exists():
            try:
                canary_result = json.loads(args.canary.read_text())
            except ValueError:
                canary_result = None
        outputs, evidence, lock = plan(ws, policy, os.environ, externals, canary_result=canary_result)
        args.plan_directory.mkdir(parents=True, exist_ok=True)
        (args.plan_directory / 'evidence.json').write_text(json.dumps(evidence, indent=1, sort_keys=True) + '\n')
        (args.plan_directory / 'Cargo.lock').write_text(lock)
        for w in outputs['warnings']:
            print(f'::warning::{w}')
        for n in outputs['notices']:
            print(f'::notice::{n}')
        _write_outputs(outputs)
        return 0
    except GuardError as exc:
        for line in str(exc).splitlines():
            print(f'::error title=Media gate classification guard::{line}')
        return 1
    except (ValueError, OSError, subprocess.CalledProcessError) as exc:
        print(f'::error title=Media gate ({args.mode})::{" ".join(str(exc).splitlines())}')
        return 1


if __name__ == '__main__':
    sys.exit(main())
