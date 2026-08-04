# RTK usage

Use RTK when it is installed to keep command output compact:

- `rtk cargo test` / `rtk cargo check` / `rtk cargo clippy` for Rust verification.
- `rtk git status`, `rtk git diff`, and `rtk git log` for repository inspection.
- `rtk grep` for large searches; plain `rg` remains preferred for normal source lookup.

Fall back to the underlying command when RTK is unavailable or exact, unfiltered output is needed
to diagnose a failure. RTK changes presentation only; do not treat it as a build dependency.
