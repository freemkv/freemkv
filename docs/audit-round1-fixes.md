# audit_round1_fixes.rs — background

## Default-template traversal gap

The DEFAULT output template is the empty one, and it was the branch the
original fix missed.

`title_basename` sanitised only inside its `{title}` substitution, so the
test `the_default_template_sanitises_the_label_too` (which passes a
`"{title}"` template) proved the traversal closed while the branch most
users are actually on -- `format!("{label}_t{n}")` with the raw label --
was still open. A class of bug is not closed by covering one of its
branches.

## Why the invariant is stated once, over all exported helpers

Round 1 fixed three seams and missed a fourth -- the ordinary drive->ISO
rip -- because the fix was driven from the findings rather than from the
list of places the label lands. The test
`every_label_derived_name_stays_one_component` states the invariant once,
over the exported helpers, so the next seam added is measured against it:
whatever the label contains, what reaches the filesystem is a single path
component.
