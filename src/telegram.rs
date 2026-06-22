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

impl TgClient {
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
            .await?
            .json()
            .await?;
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
            .await?
            .json()
            .await?;
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
            .await?
            .json()
            .await?;
        Ok(())
    }

    pub async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<()> {
        let _: serde_json::Value = self
            .http
            .post(self.url("deleteMessage"))
            .json(&json!({ "chat_id": chat_id, "message_id": message_id }))
            .send()
            .await?
            .json()
            .await?;
        Ok(())
    }
}

fn log_err<T>(resp: &TgResponse<T>) {
    if let Some(desc) = &resp.description {
        eprintln!("Telegram API error: {desc}");
    }
}
