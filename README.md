# Kumo

Kumo is a minimal, skills-based personal agent gateway written in Rust.

The initial direction is intentionally focused:

- communicate through Telegram;
- execute approved tasks on the host;
- add capabilities as small, explicit skills;
- use [Kamui](https://github.com/algonacci/kamui) as the coding skill backend.

Kumo owns communication, identity, routing, permissions, and task lifecycle. Skills own specific
capabilities. Kamui remains an independent coding agent and does not need to know about Telegram.

## Status

Kumo is at the initial scaffold stage. The first milestone is a single-user Telegram gateway with a
small skill interface and explicit host-execution permissions.

## Development

Requires a current stable Rust toolchain.

```sh
cargo run
cargo test
```

## License

MIT
