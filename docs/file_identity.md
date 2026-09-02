# `file_identity` — why this module exists, and its testing history

## Why "same file" was answered twice, and diverged

Every sink here opens its destination for writing before (or while) the
source is read, so "the destination IS the source" has to be answered before
a byte moves. It was answered twice: the CLI's `pipe` compared canonical
paths AND filesystem identity, while the GUI's `engine` compared canonical
paths alone — and the narrower copy is the one that misses a hardlink, so the
desktop app would truncate the user's only copy on a rip the CLI refuses.
Same shape as `title_identity`: one question, one answer, declared by both
crate roots.

Canonical equality is not sufficient on its own: a hardlink gives one file
two names that are each already canonical, so the paths differ while the
bytes are shared and writing either one destroys the other. That is why
`same_file` also compares the identity the filesystem itself uses
(`file_id`), not just canonical paths.

## Why the Windows `file_id` is tested unconditionally on CI

This used to carry a note saying it was type-checked but never run, because
the one test that exercises the whole point of this module — two names, one
file — was `#[cfg(unix)]`. CI runs `windows-latest` and NTFS has had hard
links since it shipped, so the cfg bought nothing and cost the only coverage
this `unsafe` FFI block could ever have had. The test is now unconditional:
`GetFileInformationByHandle` and the hand-checked `ByHandleFileInformation`
layout are executed on every Windows CI run.

`std::fs::hard_link` is cross-platform and hard links need no elevation
(unlike symlinks), so there was never a reason to skip this on Windows.

Mutation caught on Windows by the hardlink test: a wrong field order in
`ByHandleFileInformation` (the volume serial and both index halves are read
by byte offset), or a `GetFileInformationByHandle` that always answers
`None` — either makes `same_file` blind to the case where the destination is
the source under another name, and the rip overwrites its own input.

The hardlink fixture needs an NTFS `%TEMP%` on Windows (FAT32/exFAT cannot
create hard links). That failure is not skipped on purpose: skipping is what
left this module's only `unsafe` block untested for three releases.
