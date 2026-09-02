# messaging — Level + Code + Message standard

WS2 format: `Error: E<code> <message>`.

`level_for` is the single authority for the Level to code mapping.
`pipe::render_error`, `main::fatal`, the contract test, and the docs
generator all read `level_for` — do not scatter level logic elsewhere
in the codebase.
