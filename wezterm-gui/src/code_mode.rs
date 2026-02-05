use anyhow::{anyhow, Context};
use config::{AiProvider, ConfigHandle};
use keyring::Entry;

const KEYRING_SERVICE: &str = "wezterm";

#[derive(serde::Deserialize)]
pub struct PlanResponse {
    pub summary: String,
    pub mermaid: Option<String>,
    pub steps: Vec<String>,
}

pub fn request_plan(config: &ConfigHandle, task: &str) -> anyhow::Result<PlanResponse> {
    let prompt = format!(
        "Create a concise plan and a Mermaid diagram. Return JSON: \
{{\"summary\":\"...\",\"mermaid\":\"...\",\"steps\":[\"...\"]}}.\nTask: {task}"
    );
    let response = request_text(config, &prompt)?;
    let response = strip_json_fences(&response);
    serde_json::from_str(&response).context("invalid plan response JSON")
}

#[derive(serde::Deserialize)]
pub struct ActionResponse {
    pub kind: String,
    pub content: String,
    pub command: String,
}

pub fn request_action(config: &ConfigHandle, task: &str) -> anyhow::Result<ActionResponse> {
    let prompt = format!(
        "Return JSON with kind=command or code. \
If command, include a shell command string in command. \
If code, include content. \
Schema: {{\"kind\":\"command|code\",\"content\":\"...\",\"command\":\"...\"}}.\n\
User request: {task}"
    );
    let response = request_text(config, &prompt)?;
    let response = strip_json_fences(&response);
    serde_json::from_str(&response).context("invalid action response JSON")
}

fn strip_json_fences(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with("```") {
        trimmed
            .replace("```json", "")
            .replace("```", "")
            .trim()
            .to_string()
    } else {
        trimmed.to_string()
    }
}

fn request_text(config: &ConfigHandle, prompt: &str) -> anyhow::Result<String> {
    match config.ai_provider {
        AiProvider::Ollama => send_openai_compatible(
            &config.ai_ollama_base_url,
            None,
            model_or_err(config)?,
            prompt,
        ),
        AiProvider::OpenAI => {
            let key = load_api_key("openai", "WEZTERM_OPENAI_API_KEY")?;
            let key = key.ok_or_else(|| anyhow!("missing OpenAI API key"))?;
            send_openai_compatible(
                "https://api.openai.com/v1",
                Some(key),
                model_or_err(config)?,
                prompt,
            )
        }
        AiProvider::Anthropic => {
            let key = load_api_key("anthropic", "WEZTERM_ANTHROPIC_API_KEY")?;
            let key = key.ok_or_else(|| anyhow!("missing Anthropic API key"))?;
            send_anthropic(&key, model_or_err(config)?, prompt)
        }
    }
}

fn model_or_err(config: &ConfigHandle) -> anyhow::Result<String> {
    config
        .ai_model
        .clone()
        .ok_or_else(|| anyhow!("ai_model must be configured for code mode"))
}

fn load_api_key(account: &str, env_key: &str) -> anyhow::Result<Option<String>> {
    if let Ok(entry) = Entry::new(KEYRING_SERVICE, account) {
        if let Ok(value) = entry.get_password() {
            return Ok(Some(value));
        }
    }

    let env_value = std::env::var(env_key).ok();
    if let Some(value) = env_value.as_ref() {
        if let Ok(entry) = Entry::new(KEYRING_SERVICE, account) {
            let _ = entry.set_password(value);
        }
    }

    Ok(env_value)
}

fn send_openai_compatible(
    base_url: &str,
    api_key: Option<String>,
    model: String,
    message: &str,
) -> anyhow::Result<String> {
    #[derive(serde::Serialize)]
    struct Request<'a> {
        model: String,
        messages: Vec<Message<'a>>,
    }

    #[derive(serde::Serialize)]
    struct Message<'a> {
        role: &'a str,
        content: &'a str,
    }

    #[derive(serde::Deserialize)]
    struct Response {
        choices: Vec<Choice>,
    }

    #[derive(serde::Deserialize)]
    struct Choice {
        message: ChoiceMessage,
    }

    #[derive(serde::Deserialize)]
    struct ChoiceMessage {
        content: String,
    }

    let client = reqwest::blocking::Client::new();
    let mut req = client.post(format!("{base_url}/chat/completions"));
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }

    let response = req
        .json(&Request {
            model,
            messages: vec![Message {
                role: "user",
                content: message,
            }],
        })
        .send()
        .context("AI request failed")?;

    let response = response.error_for_status().context("AI request failed")?;
    let payload: Response = response.json().context("invalid AI response")?;
    let reply = payload
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("AI response missing choices"))?;
    Ok(reply.message.content)
}

fn send_anthropic(api_key: &str, model: String, message: &str) -> anyhow::Result<String> {
    #[derive(serde::Serialize)]
    struct Request<'a> {
        model: String,
        max_tokens: u32,
        messages: Vec<Message<'a>>,
    }

    #[derive(serde::Serialize)]
    struct Message<'a> {
        role: &'a str,
        content: &'a str,
    }

    #[derive(serde::Deserialize)]
    struct Response {
        content: Vec<ResponsePart>,
    }

    #[derive(serde::Deserialize)]
    struct ResponsePart {
        text: String,
    }

    let client = reqwest::blocking::Client::new();
    let response = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&Request {
            model,
            max_tokens: 1024,
            messages: vec![Message {
                role: "user",
                content: message,
            }],
        })
        .send()
        .context("Anthropic request failed")?;

    let response = response.error_for_status().context("Anthropic request failed")?;
    let payload: Response = response.json().context("invalid Anthropic response")?;
    let reply = payload
        .content
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("Anthropic response missing content"))?;
    Ok(reply.text)
}
