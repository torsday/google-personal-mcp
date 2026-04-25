use serde::{Deserialize, Serialize};

// ── Thread types ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct ThreadSummary {
    pub id: String,
    pub snippet: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct Thread {
    pub id: String,
    pub snippet: Option<String>,
    pub messages: Option<Vec<Message>>,
}

// ── Message types ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Message {
    pub id: String,
    pub thread_id: Option<String>,
    pub label_ids: Option<Vec<String>>,
    pub snippet: Option<String>,
    pub payload: Option<MessagePayload>,
    pub internal_date: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct MessagePayload {
    pub headers: Option<Vec<Header>>,
    pub body: Option<MessageBody>,
    pub parts: Option<Vec<MessagePart>>,
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct MessagePart {
    pub headers: Option<Vec<Header>>,
    pub body: Option<MessageBody>,
    pub parts: Option<Vec<Self>>,
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct MessageBody {
    pub data: Option<String>,
    pub size: Option<u64>,
}

// ── Label types ───────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct Label {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

// ── Send types ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SentMessage {
    pub id: String,
    pub thread_id: Option<String>,
    pub label_ids: Option<Vec<String>>,
}

// ── Batch result ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct BatchModifyResult {
    pub succeeded: Vec<String>,
    pub failed: Vec<FailedThread>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct FailedThread {
    pub id: String,
    pub error: String,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

impl Message {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.payload
            .as_ref()?
            .headers
            .as_ref()?
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
    }

    pub(crate) fn subject(&self) -> Option<&str> {
        self.header("Subject")
    }

    pub(crate) fn from(&self) -> Option<&str> {
        self.header("From")
    }

    pub(crate) fn date(&self) -> Option<&str> {
        self.header("Date")
    }

    /// Extract plain text body, walking multipart recursively.
    pub(crate) fn body_text(&self) -> Option<String> {
        let payload = self.payload.as_ref()?;
        extract_text_from_payload(payload)
    }
}

fn extract_text_from_payload(payload: &MessagePayload) -> Option<String> {
    if let Some(mime) = &payload.mime_type {
        if mime == "text/plain" {
            if let Some(body) = &payload.body {
                if let Some(data) = &body.data {
                    if let Ok(bytes) = base64::Engine::decode(
                        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                        data,
                    ) {
                        return String::from_utf8(bytes).ok();
                    }
                }
            }
        }
    }

    if let Some(parts) = &payload.parts {
        for part in parts {
            let pseudo = MessagePayload {
                headers: part.headers.clone(),
                body: part.body.clone(),
                parts: part.parts.clone(),
                mime_type: part.mime_type.clone(),
            };
            if let Some(text) = extract_text_from_payload(&pseudo) {
                return Some(text);
            }
        }
    }

    None
}
