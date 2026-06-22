use crate::telegram::{btn, Keyboard};

pub fn main_menu() -> Keyboard {
    vec![
        vec![btn("💰 Wallet", "wallet"), btn("📊 Positions", "positions")],
        vec![btn("🟢 Buy", "buy"), btn("🔴 Sell", "sell")],
        vec![btn("🤖 AI Tools", "ai_tools"), btn("👥 Referral", "referral")],
        vec![btn("⚙️ Settings", "settings"), btn("🔄 Refresh", "refresh")],
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

pub fn settings_menu() -> Keyboard {
    vec![
        vec![btn("🔑 Change PIN", "change_pin")],
        vec![btn("📊 Slippage", "slippage")],
        vec![btn("🏠 Main Menu", "main")],
    ]
}

pub fn slippage_menu() -> Keyboard {
    vec![
        vec![btn("1%", "slip_100"), btn("3%", "slip_300"), btn("5%", "slip_500"), btn("10%", "slip_1000")],
        vec![btn("🏠 Main Menu", "main")],
    ]
}

pub fn ai_tools_menu() -> Keyboard {
    vec![
        vec![btn("🔍 Rug Scanner", "rug_scan")],
        vec![btn("📊 Live Signals", "trade_signals")],
        vec![btn("🏠 Main Menu", "main")],
    ]
}
