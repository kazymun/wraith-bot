use crate::state::Position;
use crate::telegram::{btn, Keyboard};

pub fn main_menu() -> Keyboard {
    vec![
        vec![btn("💰 Wallet", "wallet"), btn("📊 Positions", "positions")],
        vec![btn("🟢 Buy", "buy"), btn("🔴 Sell", "sell")],
        vec![btn("🤖 AI Tools", "ai_tools"), btn("👥 Referral", "referral")],
        vec![btn("📈 PnL", "pnl"), btn("⚙️ Settings", "settings")],
        vec![btn("🔄 Refresh", "refresh")],
    ]
}

pub fn subscribe_menu(price_label: &str) -> Keyboard {
    vec![
        vec![btn(&format!("💳 Subscribe — {price_label}/mo"), "subscribe")],
        vec![btn("💰 Wallet (deposit)", "wallet"), btn("🔄 Refresh", "main")],
    ]
}

pub fn cancel_to(target: &str) -> Keyboard {
    vec![vec![btn("❌ Cancel", target)]]
}

pub fn main_only() -> Keyboard {
    vec![vec![btn("🏠 Main Menu", "main")]]
}

pub fn buy_amounts(ca: &str) -> Keyboard {
    vec![
        vec![
            btn("0.1 SOL", &format!("buyamt_{ca}_0.1")),
            btn("0.5 SOL", &format!("buyamt_{ca}_0.5")),
            btn("1 SOL", &format!("buyamt_{ca}_1")),
        ],
        vec![
            btn("5 SOL", &format!("buyamt_{ca}_5")),
            btn("10 SOL", &format!("buyamt_{ca}_10")),
            btn("✏️ X SOL", &format!("buyamt_{ca}_custom")),
        ],
        vec![btn("❌ Cancel", "main")],
    ]
}

pub fn export_key_keyboard() -> Keyboard {
    vec![
        vec![btn("🗑️ Delete This Message Now", "del_msg")],
        vec![btn("🏠 Main Menu", "main")],
    ]
}

pub fn wallet_menu() -> Keyboard {
    vec![
        vec![btn("🔄 Refresh", "wallet"), btn("⬆️ Withdraw", "withdraw")],
        vec![btn("🔑 Export Private Key", "export_key"), btn("📥 Import Wallet", "import_wallet")],
        vec![btn("🏠 Main Menu", "main")],
    ]
}

pub fn settings_menu(gem_alerts: bool) -> Keyboard {
    let gem_label = if gem_alerts { "🔔 Gem Alerts: ON" } else { "🔕 Gem Alerts: OFF" };
    vec![
        vec![btn("🔑 Change PIN", "change_pin")],
        vec![btn("📊 Slippage", "slippage")],
        vec![btn(gem_label, "toggle_gem_alerts")],
        vec![btn("🏠 Main Menu", "main")],
    ]
}

pub fn slippage_menu() -> Keyboard {
    vec![
        vec![btn("1%", "slip_100"), btn("3%", "slip_300"), btn("5%", "slip_500"), btn("10%", "slip_1000")],
        vec![btn("🏠 Main Menu", "main")],
    ]
}

/// One button per open position, so selling is "tap the coin" instead of
/// pasting its CA. Callback data is `sellsel_<mint>` (mint addresses are
/// well under Telegram's 64-byte callback_data limit on their own).
pub fn position_list(positions: &[Position]) -> Keyboard {
    let mut kb: Keyboard = positions
        .iter()
        .map(|p| {
            vec![btn(
                &format!("🪙 {} — {:.4} SOL in", p.symbol, p.sol_spent),
                &format!("sellsel_{}", p.mint),
            )]
        })
        .collect();
    kb.push(vec![btn("❌ Cancel", "main")]);
    kb
}

/// How much of a held token to sell, as a percentage of current balance.
pub fn sell_percent_menu(ca: &str) -> Keyboard {
    vec![
        vec![
            btn("25%", &format!("sellpct_{ca}_25")),
            btn("50%", &format!("sellpct_{ca}_50")),
            btn("75%", &format!("sellpct_{ca}_75")),
        ],
        vec![
            btn("💯 100% (All)", &format!("sellpct_{ca}_100")),
            btn("✏️ Custom %", &format!("sellpct_{ca}_custom")),
        ],
        vec![btn("❌ Cancel", "main")],
    ]
}

pub fn ai_tools_menu() -> Keyboard {
    vec![
        vec![btn("🔍 Rug Scanner", "rug_scan")],
        vec![btn("📊 Live Signals", "trade_signals")],
        vec![btn("💎 AI Gem Scanner", "gem_scan")],
        vec![btn("🏠 Main Menu", "main")],
    ]
}
