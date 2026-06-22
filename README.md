# Wraith

Solana memecoin Telegram bot: real per-user custodial wallets, live DexScreener
token stats with a heuristic rug-risk score, and real swaps through Jupiter's
aggregator (the same one Trojan/BananaGun/BonkBot route through).

## What's actually real here

Every wallet is a genuine Solana keypair generated server-side (not a random
string). Private keys are encrypted at rest with AES-256-GCM under a master
key you control. Balances are read live via RPC. Buys/sells get a real quote
from Jupiter, build a real transaction, sign it with the user's stored key,
and broadcast it. Withdrawals build and sign a real `SystemProgram::transfer`.
Nothing in here fakes a balance or a trade confirmation.

## Setup

1. Install Rust: https://rustup.rs
2. Copy `.env.example` to `.env` and fill it in:
   - `TELEGRAM_BOT_TOKEN` from @BotFather
   - `SOLANA_RPC_URL` — get a real RPC endpoint from Helius, QuickNode, or
     Triton. The public mainnet RPC is rate-limited and will fail under load.
   - `WRAITH_MASTER_KEY` — generate with `openssl rand -base64 32`
3. `cargo run`

## Security model (read this before letting anyone else use it)

This is a **custodial** bot — it holds private keys on behalf of users. That
means:

- Whoever has `WRAITH_MASTER_KEY` can decrypt every stored wallet. Treat it
  like a root password: never commit it, never log it, store it in a secrets
  manager in production rather than a plain `.env` file on a server.
- If you lose the master key, every wallet's funds are permanently
  unrecoverable. Back it up somewhere safe and offline.
- Users should be told plainly that this is custodial and who controls it.
  The export-key feature exists specifically so users aren't trapped — make
  sure it's easy to find.
- Depending on your jurisdiction, running a service that custodies other
  people's funds may carry legal/regulatory obligations (money transmission,
  KYC/AML, etc). That's a question for an actual lawyer, not this README —
  worth checking before opening it up beyond a small trusted group.
- PIN protection on export/withdraw is a UX speed bump, not real security —
  it's stored as a salted-less SHA-256 hash. It deters a casual phone-grab,
  not a database compromise.

## Known limitations / next steps

- **Sniping new launches** isn't implemented as true real-time detection yet.
  A real implementation needs to subscribe to program logs (Raydium pool
  creation, pump.fun) via a websocket/Geyser feed and react within
  milliseconds. Right now "buy" only works for tokens that already have
  liquidity on a DexScreener-indexed pair.
- No MEV protection (Jito bundles) yet — transactions go through the public
  path and can be sandwiched. Worth adding before trading meaningful size.
- The Jupiter-returned transaction has a blockhash baked in at quote time; if
  there's a long delay before signing/sending it can expire. Add a
  refresh-and-retry loop if you see failures.
- `find_by_ref_code` is a linear scan over the whole DB — fine for hundreds
  of users, replace with an index if this gets to thousands.

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
