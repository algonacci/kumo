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

Kumo currently provides a single-user Telegram bot that receives text messages and sends a hardcoded
reply. Skills and host execution are not implemented yet.

## Telegram setup

1. Create a bot through [@BotFather](https://t.me/BotFather) and copy its token.
2. Get your numeric Telegram user ID, for example through [@userinfobot](https://t.me/userinfobot).
3. Copy `.env.example` to `.env` and replace both placeholder values.
4. Run Kumo with `cargo run`.

PowerShell:

```powershell
Copy-Item .env.example .env
cargo run
```

Bash:

```sh
cp .env.example .env
cargo run
```

The `.env` file is ignored by Git and must not be committed. Send a text message to the bot after it
starts. Kumo ignores messages from every other Telegram user.

## Development

Requires a current stable Rust toolchain.

```sh
cargo run
cargo test
```

## License

MIT
