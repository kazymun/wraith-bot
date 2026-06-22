mod config;
mod crypto;
mod db;
mod dexscreener;
mod handlers;
mod jupiter;
mod keyboards;
mod rpc;
mod state;
mod telegram;
mod wallet;

use config::Config;
use crypto::Crypto;
use db::Db;
use handlers::App;
use jupiter::Jupiter;
use rpc::SolanaRpc;
use telegram::TgClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load()?;

    let app = App {
        tg: TgClient::new(cfg.telegram_token.clone()),
        db: Db::open(&cfg.db_path)?,
        crypto: Crypto::new(&cfg.master_key_b64)?,
        rpc: SolanaRpc::new(cfg.rpc_url.clone()),
        jup: Jupiter::new(),
        default_slippage_bps: cfg.default_slippage_bps,
        fee_wallet: cfg.fee_wallet.clone(),
    };

    println!("👻 Wraith bot is running...");

    let mut offset: i64 = 0;
    loop {
        let updates = match app.tg.get_updates(offset).await {
            Ok(u) => u,
            Err(e) => {
                eprintln!("Failed to get updates: {e}. Retrying in 3s...");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                continue;
            }
        };

        for update in updates {
            offset = update.update_id + 1;

            if let Some(msg) = update.message {
                if let Err(e) = app.handle_message(msg).await {
                    eprintln!("Error handling message: {e}");
                }
            } else if let Some(cb) = update.callback_query {
                let chat_id = cb.message.as_ref().map(|m| m.chat.id);
                let message_id = cb.message.as_ref().map(|m| m.message_id);
                if let Some(chat_id) = chat_id {
                    if let Some(data) = &cb.data {
                        if let Err(e) = app.handle_callback(&cb.id, data, chat_id, cb.from.id, message_id).await {
                            eprintln!("Error handling callback: {e}");
                        }
                    }
                }
            }
        }
    }
}
