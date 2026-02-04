use anyhow::{anyhow, Context};
use config::{AiProvider, ConfigHandle};
use keyring::Entry;
use mux::window::WindowId;
use mux::pane::Pane;
use mux::termwiztermtab::TermWizTerminal;
use std::sync::Arc;
use termwiz::lineedit::{Action, BasicHistory, History, LineEditor, LineEditorHost};
use termwiz::surface::Change;
use termwiz::terminal::Terminal;
use crate::ollama;
use wezterm_term::{TerminalConfiguration, TerminalSize};

const KEYRING_SERVICE: &str = "wezterm";
const AI_SYSTEM_PROMPT: &str = "You are a terminal assistant. \
If the user asks for a shell action, respond with exactly one line in the form \
\"command: <shell command>\" and nothing else. \
Do not include code fences. \
Otherwise, answer concisely in 1-3 sentences.";

pub fn spawn_ai_tab_in_window(
    size: TerminalSize,
    window_id: Option<WindowId>,
    config: ConfigHandle,
) -> anyhow::Result<()> {
    let term_config: Arc<dyn TerminalConfiguration + Send + Sync> =
        Arc::new(config::TermConfig::with_config(config.clone()));
    let run_config = config.clone();
    promise::spawn::spawn(async move {
        let _ = mux::termwiztermtab::run(
            size,
            window_id,
            move |term| run_ai_loop(term, run_config),
            Some(term_config),
        )
        .await;
    })
    .detach();
    Ok(())
}

pub fn spawn_ai_pane(
    size: TerminalSize,
    config: ConfigHandle,
) -> anyhow::Result<Arc<dyn Pane>> {
    let term_config: Arc<dyn TerminalConfiguration + Send + Sync> =
        Arc::new(config::TermConfig::with_config(config.clone()));
    let run_config = config.clone();
    let (term, pane) = mux::termwiztermtab::allocate(size, term_config);
    std::thread::spawn(move || {
        let _ = run_ai_loop(term, run_config);
    });
    Ok(pane)
}

struct AiHost {
    history: BasicHistory,
}

impl AiHost {
    fn new() -> Self {
        Self {
            history: BasicHistory::default(),
        }
    }
}

impl LineEditorHost for AiHost {
    fn history(&mut self) -> &mut dyn History {
        &mut self.history
    }

    fn resolve_action(
        &mut self,
        event: &termwiz::input::InputEvent,
        editor: &mut LineEditor<'_>,
    ) -> Option<Action> {
        let (line, _cursor) = editor.get_line_and_cursor();
        if line.is_empty()
            && matches!(
                event,
                termwiz::input::InputEvent::Key(termwiz::input::KeyEvent {
                    key: termwiz::input::KeyCode::Escape,
                    ..
                })
            )
        {
            Some(Action::Cancel)
        } else {
            None
        }
    }
}

pub fn run_ai_loop(mut term: TermWizTerminal, config: ConfigHandle) -> anyhow::Result<()> {
    term.no_grab_mouse_in_raw_mode();
    term.render(&[Change::Text(
        "AI mode: type a prompt and press Enter. Use /exit to close.\r\n".to_string(),
    )])?;

    let mut host = AiHost::new();
    let mut ollama_ready = false;
    loop {
        let mut editor = LineEditor::new(&mut term);
        editor.set_prompt("ai> ");
        let line = editor.read_line(&mut host)?;
        let Some(line) = line else {
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "/exit" {
            break;
        }

        if matches!(config.ai_provider, AiProvider::Ollama) && !ollama_ready {
            let result = ollama::ensure_ollama_ready(
                &config.ai_ollama_base_url,
                config.ai_model.as_deref(),
                |message| {
                let _ = term.render(&[Change::Text(format!("{message}\r\n"))]);
                },
            );
            if let Err(err) = result {
                term.render(&[Change::Text(format!("Error: {err:#}\r\n"))])?;
                continue;
            }
            ollama_ready = true;
        }

        match send_message(&config, line) {
            Ok(reply) => {
                if let Some(command) = extract_command(&reply).or_else(|| infer_command(&reply)) {
                    term.render(&[Change::Text(format!("Running: {command}\r\n"))])?;
                    match execute_command(&command) {
                        Ok(output) => term.render(&[Change::Text(output + "\r\n")])?,
                        Err(err) => {
                            term.render(&[Change::Text(format!("Error: {err:#}\r\n"))])?;
                        }
                    }
                } else {
                    term.render(&[Change::Text(format!("{reply}\r\n"))])?;
                }
            }
            Err(err) => {
                term.render(&[Change::Text(format!("Error: {err:#}\r\n"))])?;
            }
        }
    }

    Ok(())
}

fn send_message(config: &ConfigHandle, message: &str) -> anyhow::Result<String> {
    match config.ai_provider {
        AiProvider::Ollama => send_openai_compatible(
            &config.ai_ollama_base_url,
            None,
            model_or_err(config)?,
            message,
        ),
        AiProvider::OpenAI => {
            let key = load_api_key("openai", "WEZTERM_OPENAI_API_KEY")?;
            let key = key.ok_or_else(|| anyhow!("missing OpenAI API key"))?;
            send_openai_compatible(
                "https://api.openai.com/v1",
                Some(key),
                model_or_err(config)?,
                message,
            )
        }
        AiProvider::Anthropic => {
            let key = load_api_key("anthropic", "WEZTERM_ANTHROPIC_API_KEY")?;
            let key = key.ok_or_else(|| anyhow!("missing Anthropic API key"))?;
            send_anthropic(&key, model_or_err(config)?, message)
        }
    }
}

fn model_or_err(config: &ConfigHandle) -> anyhow::Result<String> {
    config
        .ai_model
        .clone()
        .ok_or_else(|| anyhow!("ai_model must be configured for AI mode"))
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
            messages: vec![
                Message {
                    role: "system",
                    content: AI_SYSTEM_PROMPT,
                },
                Message {
                    role: "user",
                    content: message,
                },
            ],
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
        system: &'a str,
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
            system: AI_SYSTEM_PROMPT,
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

fn extract_command(reply: &str) -> Option<String> {
    let trimmed = reply.trim();
    if let Some(command) = trimmed.strip_prefix("command:") {
        let command = command.trim();
        if !command.is_empty() {
            return Some(command.to_string());
        }
    }
    None
}

fn infer_command(reply: &str) -> Option<String> {
    let trimmed = reply.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    let is_simple = !trimmed.contains(' ') && trimmed.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '-' | '_' | ':' )
    });
    if is_simple {
        return Some(trimmed.to_string());
    }
    None
}

fn execute_command(command: &str) -> anyhow::Result<String> {
    use std::process::Command;
    let output = if cfg!(windows) {
        Command::new("cmd")
            .arg("/C")
            .arg(command)
            .output()
            .context("failed to run command")?
    } else {
        Command::new("/bin/sh")
            .arg("-lc")
            .arg(command)
            .output()
            .context("failed to run command")?
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let mut combined = String::new();
    if !stdout.is_empty() {
        combined.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    if combined.is_empty() {
        combined = "<no output>".to_string();
    }
    Ok(normalize_line_endings(&combined))
}

fn normalize_line_endings(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n");
    normalized.replace('\n', "\r\n")
}
