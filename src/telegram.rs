use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

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

    pub fn new(token: String) -> Self {
        Self {
            token,
            http: reqwest::Client::new(),
        }
    }

    fn url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.token, method)
    }

    pub async fn get_updates(&self, offset: i64) -> Result<Vec<TgUpdate>> {
        let resp: TgResponse<Vec<TgUpdate>> = self
            .http
            .get(self.url("getUpdates"))
            .query(&[("offset", offset.to_string()), ("timeout", "30".to_string())])
            .send()
            .await
            .map_err(|e| self.redact(e.to_string()))?
            .json()
            .await
            .map_err(|e| self.redact(e.to_string()))?;
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
            .map_err(|e| self.redact(e.to_string()))?
            .json()
            .await
            .map_err(|e| self.redact(e.to_string()))?;
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
            .map_err(|e| self.redact(e.to_string()))?
            .json()
            .await
            .map_err(|e| self.redact(e.to_string()))?;
        Ok(())
    }

    pub async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<()> {
        let _: serde_json::Value = self
            .http
            .post(self.url("deleteMessage"))
            .json(&json!({ "chat_id": chat_id, "message_id": message_id }))
            .send()
            .await
            .map_err(|e| self.redact(e.to_string()))?
            .json()
            .await
            .map_err(|e| self.redact(e.to_string()))?;
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
