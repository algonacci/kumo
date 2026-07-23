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

Kumo currently provides a single-user Telegram bot backed by an OpenAI-compatible model provider.
Conversation history, skills, and host execution are not implemented yet.

## Onboarding

Run Kumo without arguments:

```sh
cargo run
```

On first run, Kumo starts an interactive setup that:

- opens [@BotFather](https://t.me/BotFather) to create a private bot;
- asks for the bot token without displaying it in the terminal;
- validates the token with Telegram;
- opens the new bot with a one-time pairing link;
- detects the owner's Telegram user ID when they tap **Start**;
- asks for an OpenAI-compatible provider URL and API key;
- discovers the provider's available models and lets the user choose one;
- saves everything to the OS config directory as `kumo/kumo.toml`.

No `.env` file or manual user ID lookup is required. The pairing nonce ensures that an unrelated
Telegram user cannot claim the bot by messaging it first. Kumo ignores messages from every account
except the paired owner.

Existing Telegram-only installations are upgraded in place and only ask for provider settings. Run
onboarding again at any time to replace the provider settings:

```sh
cargo run -- onboard
```

The bot token and provider API key are stored in the user's global `kumo.toml`. On Unix, Kumo
restricts this file to the current user (`0600`). Never publish or commit this file. API keys may be
left empty for local OpenAI-compatible servers that do not require authentication.

## Telegram commands

- `/model` shows the active model.
- `/models` lists models discovered during onboarding.
- `/model <id>` switches the active model and saves the choice.

Normal text messages are sent to the active model. The current MVP treats every message as an
independent request; conversation history will be added separately.

## Development

Requires a current stable Rust toolchain.

```sh
cargo run
cargo test
```

## License

MIT
