use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::error::Error as _;

#[derive(Clone)]
pub struct TgClient {
    token: String,
    http: reqwest::Client,
}

#[derive(Debug, Deserialize)]
pub struct TgUser {
    pub id: i64,
    pub first_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TgChat {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct TgMessage {
    pub message_id: i64,
    pub chat: TgChat,
    pub text: Option<String>,
    pub from: Option<TgUser>,
}

#[derive(Debug, Deserialize)]
pub struct TgCallbackQuery {
    pub id: String,
    pub from: TgUser,
    pub message: Option<TgMessage>,
    pub data: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TgUpdate {
    pub update_id: i64,
    pub message: Option<TgMessage>,
    pub callback_query: Option<TgCallbackQuery>,
}

#[derive(Debug, Deserialize)]
struct TgResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct InlineButton {
    pub text: String,
    pub callback_data: String,
}

pub fn btn(text: &str, data: &str) -> InlineButton {
    InlineButton {
        text: text.to_string(),
        callback_data: data.to_string(),
    }
}

pub type Keyboard = Vec<Vec<InlineButton>>;

/// A wrapper around anyhow::Error that guarantees the bot token can never
/// appear in its Display output, even if the underlying reqwest error
/// includes the full request URL (which contains `bot<TOKEN>/...`).
/// ALWAYS use this (never the raw reqwest/anyhow error) anywhere a
/// TgClient error might get logged, eprintln'd, or shown to a user.
#[derive(Debug)]
pub struct SafeTgError(String);

impl std::fmt::Display for SafeTgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for SafeTgError {}

impl TgClient {
    fn redact(&self, s: impl AsRef<str>) -> SafeTgError {
        SafeTgError(s.as_ref().replace(&self.token, "[REDACTED_BOT_TOKEN]"))
    }

    /// Like `redact`, but walks the full `.source()` chain of the error
    /// first. reqwest's top-level message alone ("error sending request
    /// for url (...)") tells you almost nothing -- the actual reason
    /// (DNS failure, TLS/certificate error, connection refused, timed
    /// out, etc.) lives further down the chain and was previously being
    /// thrown away by calling `.to_string()` on just the outer error.
    fn redact_chain(&self, e: &reqwest::Error) -> SafeTgError {
        let mut msg = e.to_string();
        let mut cur: Option<&(dyn std::error::Error + 'static)> = e.source();
        while let Some(s) = cur {
            msg.push_str(" — caused by: ");
            msg.push_str(&s.to_string());
            cur = s.source();
        }
        self.redact(msg)
    }

    pub fn new(token: String) -> Self {
        Self {
            token,
            // NOTE: previously forced to HTTP/1.1 to work around an
            // "invalid HTTP version parsed" error some deployments hit
            // during ALPN/h2 negotiation. On other networks that forced
            // downgrade is itself what triggers the same error (curl on
            // the same machine negotiates HTTP/2 with Telegram cleanly).
            // Letting reqwest auto-negotiate (its default) works for
            // both cases -- if you hit "invalid HTTP version parsed"
            // again on some future host, that's a local network/proxy
            // issue to chase, not something to paper over by forcing a
            // specific HTTP version here.
            http: reqwest::Client::builder()
                .build()
                .expect("failed to build reqwest client"),
        }
    }

    fn url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.token, method)
    }

    /// IMPORTANT: unlike a "just show nothing happened yet" no-op, an
    /// invalid/revoked bot token makes Telegram return `{"ok": false,
    /// "error_code": 401, ...}` here -- and until this fix, that response
    /// was silently treated as "no new messages" (`resp.result` is `None`
    /// on a failed call, and `.unwrap_or_default()` turned that into an
    /// empty `Vec` with no error at all). That made a dead token
    /// indistinguishable from a perfectly healthy, quiet bot: clean
    /// startup logs, no errors anywhere, and zero messages ever
    /// delivered. Now this actually checks `ok` and returns `Err`, so
    /// main.rs's existing "Failed to get updates: {e}" logging catches
    /// it loudly instead of hiding it forever.
    pub async fn get_updates(&self, offset: i64) -> Result<Vec<TgUpdate>> {
        let resp: TgResponse<Vec<TgUpdate>> = self
            .http
            .get(self.url("getUpdates"))
            .query(&[("offset", offset.to_string()), ("timeout", "30".to_string())])
            .send()
            .await
            .map_err(|e| self.redact_chain(&e))?
            .json()
            .await
            .map_err(|e| self.redact_chain(&e))?;
        if !resp.ok {
            let desc = resp.description.unwrap_or_else(|| "unknown error".to_string());
            return Err(anyhow!("getUpdates failed: {desc} -- check TELEGRAM_BOT_TOKEN is valid and not revoked"));
        }
        Ok(resp.result.unwrap_or_default())
    }

    pub async fn send_html(&self, chat_id: i64, text: &str, keyboard: Option<Keyboard>) -> Result<Option<i64>> {
        let mut body = json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML",
        });
        if let Some(kb) = keyboard {
            body["reply_markup"] = json!({ "inline_keyboard": kb });
        }
        let resp: TgResponse<serde_json::Value> = self
            .http
            .post(self.url("sendMessage"))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.redact_chain(&e))?
            .json()
            .await
            .map_err(|e| self.redact_chain(&e))?;
        if !resp.ok {
            log_err(&resp);
            return Ok(None);
        }
        Ok(resp.result.as_ref().and_then(|r| r["message_id"].as_i64()))
    }

    pub async fn answer_callback(&self, callback_id: &str, text: Option<&str>) -> Result<()> {
        let mut body = json!({ "callback_query_id": callback_id });
        if let Some(t) = text {
            body["text"] = json!(t);
        }
        let _: serde_json::Value = self
            .http
            .post(self.url("answerCallbackQuery"))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.redact_chain(&e))?
            .json()
            .await
            .map_err(|e| self.redact_chain(&e))?;
        Ok(())
    }

    /// IMPORTANT: unlike a generic "fire and forget" API call, callers of
    /// this specifically rely on knowing whether the delete actually
    /// happened -- this is how Wraith scrubs PINs and exported private
    /// keys out of chat history. Telegram's `deleteMessage` can and does
    /// fail (message older than 48h, already deleted, bot lacks admin
    /// rights in a group, etc) and returns `"ok": false` with a
    /// description when it does. Previously this function discarded that
    /// response body and always returned `Ok(())` regardless -- meaning a
    /// failed delete of a sensitive message looked identical to a
    /// successful one to every caller. Now it actually checks `ok` and
    /// returns `Err` on failure, so callers (see handlers.rs) can warn
    /// the user their sensitive message is still sitting in the chat.
    pub async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<()> {
        let resp: TgResponse<serde_json::Value> = self
            .http
            .post(self.url("deleteMessage"))
            .json(&json!({ "chat_id": chat_id, "message_id": message_id }))
            .send()
            .await
            .map_err(|e| self.redact_chain(&e))?
            .json()
            .await
            .map_err(|e| self.redact_chain(&e))?;
        if !resp.ok {
            let desc = resp.description.unwrap_or_else(|| "unknown error".to_string());
            return Err(anyhow!("deleteMessage failed: {desc}"));
        }
        Ok(())
    }
}

fn log_err<T>(resp: &TgResponse<T>) {
    if let Some(desc) = &resp.description {
        // Telegram's own error descriptions don't contain your token,
        // so this one's safe to print as-is.
        eprintln!("Telegram API error: {desc}");
    }
}

/// Escapes text that will be interpolated into a `parse_mode: HTML` message
/// but did NOT originate from our own hardcoded strings -- e.g. token
/// names/symbols pulled from DexScreener or PumpPortal. Telegram's HTML
/// parser treats bare `<`, `>`, and `&` as the start of markup; a token
/// name containing any of these (extremely common with meme coins doing
/// things like "APE<>DOGE") breaks the parser and the ENTIRE message --
/// including any inline keyboard/buttons attached to it -- silently fails
/// to send. Always wrap untrusted text with this before formatting it into
/// an HTML message. Do NOT use this on strings we already hand-wrote with
/// intentional tags like <b>/<code>/<i> -- only on the untrusted values
/// substituted into them.
pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
