Wraith — Solana Trading Bot

A high-performance, Rust-based Telegram bot for trading Solana memecoins. Wraith integrates real-time blockchain data, secure wallet management, and AI-driven risk analysis to execute trades faster than standard DEX interfaces.

Core Features

Real-time Sniping: Connects directly to PumpPortal's WebSocket firehose to detect and execute trades on new pump.fun tokens before DexScreener indexes them.
Secure Wallet Management: Users can generate or import Solana keypairs. Private keys are encrypted at rest using AES-256-GCM.
Advanced Key Derivation: Employs Argon2id for PIN-based key derivation, utilizing a server-side pepper and unique per-user salts. No master key is stored, meaning database leaks do not compromise user wallets.
Jupiter Swap Integration: Executes quotes and swaps via Jupiter API, with dynamic slippage and platform fee routing.
AI Rug Scanner: Background job that pulls DexScreener market data and on-chain checks (mint/freeze authority, holder concentration) to generate a 0-100 safety score.

Tech Stack

Language: Rust 
Blockchain: Solana (RPC, Jupiter API, PumpPortal WebSockets)
Security: AES-256-GCM, Argon2id, Envelope Encryption
Database: sled (embedded key-value store)
Architecture Overview
main.rs / state.rs: Boots the bot, handles Telegram long-polling, and manages the user state machine.
crypto.rs / wallet.rs: Implements envelope encryption (DEK/KEK) and Argon2id hashing.
jupiter.rs: Handles Jupiter API quote fetching and fee-account routing for wrapped SOL.
dexscreener.rs / pumpportal.rs: Live data ingestion and heuristic scoring.

Setup

TELEGRAM_BOT_TOKEN: Your Telegram Bot API token.
SOLANA_RPC_URL: Your Solana RPC endpoint.
WRAITH_PEPPER: A random 32-byte base64 string used for key derivation.
## Pushing to your GitHub repo

You already created `pr1deperp/wraith-bot` on GitHub. To get this code in:

```bash
git clone https://github.com/pr1deperp/wraith-bot.git
cd wraith-bot
# copy in all files from this project (everything except target/ and .env)
git add .
git commit -m "Initial Wraith bot: real wallets, real Jupiter swaps"
git push
```

Make sure `.env` is never committed — add it to `.gitignore` (see below).
