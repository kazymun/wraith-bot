use crate::state::{Position, UserRecord, MAX_WALLETS};
use crate::telegram::{btn, Keyboard};

pub fn main_menu() -> Keyboard {
    vec![
        vec![btn("💰 Wallet", "wallet"), btn("📊 Positions", "positions")],
        vec![btn("🟢 Buy", "buy"), btn("🔴 Sell", "sell")],
        vec![btn("🤖 AI Tools", "ai_tools"), btn("👥 Referral", "referral")],
        vec![btn("📈 PnL", "pnl"), btn("⚙️ Settings", "settings")],
        vec![btn("🌱 Yield", "yield")],
        vec![btn("🔄 Refresh", "refresh")],
    ]
}

pub fn subscribe_menu(price_label: &str) -> Keyboard {
    vec![
        vec![btn(&format!("💳 Subscribe — {price_label}/mo"), "subscribe")],
        vec![btn("💰 Wallet (deposit)", "wallet"), btn("🔄 Refresh", "main")],
    ]
}

/// Amount-selection + unstake keyboard for the yield feature. `has_stake`
/// controls whether "Unstake All" appears (no point showing it if the
/// user has nothing currently staked).
pub fn yield_menu(has_stake: bool) -> Keyboard {
    let mut kb: Keyboard = vec![
        vec![
            btn("0.5 SOL", "yieldamt_0.5"),
            btn("1 SOL", "yieldamt_1"),
            btn("5 SOL", "yieldamt_5"),
        ],
        vec![btn("✏️ Custom SOL", "yieldamt_custom")],
    ];
    if has_stake {
        kb.push(vec![btn("💸 Unstake All", "yield_unstake")]);
    }
    kb.push(vec![btn("🏠 Main Menu", "main")]);
    kb
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

pub fn wallet_menu(user: &UserRecord) -> Keyboard {
    let switcher_label = if user.wallets.len() > 1 {
        format!("🔀 Switch Wallet ({})", user.active().label)
    } else {
        "🔀 Add Wallet".to_string()
    };
    vec![
        vec![btn("🔄 Refresh", "wallet"), btn("⬆️ Withdraw", "withdraw")],
        vec![btn("🔑 Export Private Key", "export_key"), btn("📥 Import Wallet", "import_wallet")],
        vec![btn(&switcher_label, "wallet_switch")],
        vec![btn("🏠 Main Menu", "main")],
    ]
}

/// One button per existing wallet slot (tap to make it active), plus an
/// "add wallet" row unless the user has hit MAX_WALLETS. Callback data is
/// `walletsel_<index>` for switching, `wallet_add` for adding.
pub fn wallet_switcher(user: &UserRecord) -> Keyboard {
    let mut kb: Keyboard = user
        .wallets
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let mark = if i == user.active_wallet { "✅ " } else { "" };
            vec![btn(
                &format!("{mark}{} — {}", w.label, short_display(&w.pubkey)),
                &format!("walletsel_{i}"),
            )]
        })
        .collect();
    if user.wallets.len() < MAX_WALLETS {
        kb.push(vec![btn("➕ Add Wallet", "wallet_add")]);
    }
    kb.push(vec![btn("⬅️ Back", "wallet")]);
    kb
}

/// Choice of how to add a new wallet slot.
pub fn add_wallet_menu() -> Keyboard {
    vec![
        vec![btn("🆕 Generate New", "wallet_add_new")],
        vec![btn("📥 Import Existing", "wallet_add_import")],
        vec![btn("⬅️ Back", "wallet_switch")],
    ]
}

/// Local truncated-address helper so this module doesn't need to import
/// handlers.rs's private `short_wallet` -- same 4+4 truncation.
pub fn short_display(w: &str) -> String {
    if w.len() < 8 {
        return w.to_string();
    }
    format!("{}...{}", &w[..4], &w[w.len() - 4..])
}

pub fn settings_menu(gem_alerts: bool, yield_auto_enabled: bool) -> Keyboard {
    let gem_label = if gem_alerts { "🔔 Gem Alerts: ON" } else { "🔕 Gem Alerts: OFF" };
    let auto_yield_label = if yield_auto_enabled { "🌾 Auto-Yield: ON" } else { "🌾 Auto-Yield: OFF" };
    vec![
        vec![btn("🔑 Change PIN", "change_pin")],
        vec![btn("📊 Slippage", "slippage")],
        vec![btn(gem_label, "toggle_gem_alerts")],
        vec![btn(auto_yield_label, "toggle_auto_yield")],
        vec![btn("🗑️ Reset Account", "reset_account")],
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
