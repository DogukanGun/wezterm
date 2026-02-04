use anyhow::{anyhow, Context};
use config::{AiProvider, ConfigHandle};
use keyring::Entry;
use mux::pane::Pane;
use mux::termwiztermtab::TermWizTerminal;
use mux::window::WindowId;
use std::process::Command;
use std::sync::Arc;
use termwiz::lineedit::{BasicHistory, History, LineEditor, LineEditorHost};
use termwiz::surface::{Change, Position};
use termwiz::terminal::Terminal;
use wezterm_term::{TerminalConfiguration, TerminalSize};
use crate::mermaid_overlay;
use crate::ollama;

const KEYRING_SERVICE: &str = "wezterm";

pub fn spawn_code_mode_tab_in_window(
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
            move |term| run_code_mode_loop(term, run_config),
            Some(term_config),
        )
        .await;
    })
    .detach();
    Ok(())
}

pub fn spawn_code_mode_pane(
    size: TerminalSize,
    config: ConfigHandle,
) -> anyhow::Result<Arc<dyn Pane>> {
    let term_config: Arc<dyn TerminalConfiguration + Send + Sync> =
        Arc::new(config::TermConfig::with_config(config.clone()));
    let run_config = config.clone();
    let (term, pane) = mux::termwiztermtab::allocate(size, term_config);
    std::thread::spawn(move || {
        let _ = run_code_mode_loop(term, run_config);
    });
    Ok(pane)
}

#[derive(Default)]
struct CodeSession {
    plan_summary: Option<String>,
    plan_mermaid: Option<String>,
    steps: Vec<String>,
    current_step: Option<usize>,
    last_action: Option<String>,
    ollama_ready: bool,
}

struct CodeHost {
    history: BasicHistory,
}

impl CodeHost {
    fn new() -> Self {
        Self {
            history: BasicHistory::default(),
        }
    }
}

impl LineEditorHost for CodeHost {
    fn history(&mut self) -> &mut dyn History {
        &mut self.history
    }
}

pub fn run_code_mode_loop(mut term: TermWizTerminal, config: ConfigHandle) -> anyhow::Result<()> {
    term.no_grab_mouse_in_raw_mode();
    let mut host = CodeHost::new();
    let mut session = CodeSession::default();

    term.render(&[Change::Text(
        "Code mode: use /plan <task> to create a plan. Use /exit to close.\r\n".to_string(),
    )])?;

    loop {
        render_process_map(&mut term, &session)?;
        let mut editor = LineEditor::new(&mut term);
        editor.set_prompt("code> ");
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

        if let Some(task) = line.strip_prefix("/plan ") {
            if matches!(config.ai_provider, AiProvider::Ollama) && !session.ollama_ready {
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
                session.ollama_ready = true;
            }

            let plan = request_plan(&config, task)?;
            session.plan_summary = Some(plan.summary.clone());
            session.plan_mermaid = plan.mermaid.clone();
            session.steps = plan.steps.clone();
            session.current_step = session.steps.is_empty().then_some(0);
            session.last_action = Some("plan generated".to_string());

            term.render(&[
                Change::Text(format!("\r\nPlan:\r\n{}\r\n", plan.summary)),
                Change::Text(plan.mermaid.unwrap_or_default() + "\r\n"),
            ])?;
            if let Some(diagram) = session.plan_mermaid.as_ref() {
                mermaid_overlay::show_mermaid(diagram);
            }
            continue;
        }

        if matches!(config.ai_provider, AiProvider::Ollama) && !session.ollama_ready {
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
            session.ollama_ready = true;
        }

        let result = request_action(&config, line)?;
        match result.kind.as_str() {
            "command" => {
                session.last_action = Some("command suggested".to_string());
                term.render(&[Change::Text(format!(
                    "Suggested command:\r\n{}\r\n",
                    result.command
                ))])?;

                let should_run = if config.code_mode_auto_accept {
                    true
                } else {
                    prompt_bool(&mut term, &mut host, "Run command? (y/N)", false)?
                };

                if should_run {
                    session.last_action = Some("command executed".to_string());
                    let output = execute_command(&result.command)?;
                    term.render(&[Change::Text(output + "\r\n")])?;
                } else {
                    session.last_action = Some("command skipped".to_string());
                }
            }
            _ => {
                session.last_action = Some("code response".to_string());
                term.render(&[Change::Text(format!("{}\r\n", result.content))])?;
            }
        }
    }

    Ok(())
}

fn render_process_map(term: &mut TermWizTerminal, session: &CodeSession) -> anyhow::Result<()> {
    let screen = term.get_screen_size()?;
    let cols = screen.cols;
    let width = 32usize.min(cols.saturating_sub(1).max(1));
    let start_x = cols.saturating_sub(width);
    let mut lines = Vec::new();
    lines.push("Process".to_string());
    if let Some(step) = session.current_step {
        let label = session
            .steps
            .get(step)
            .cloned()
            .unwrap_or_else(|| "N/A".to_string());
        lines.push(format!("Step: {}: {}", step + 1, label));
    } else {
        lines.push("Step: -".to_string());
    }
    lines.push(format!(
        "Last: {}",
        session
            .last_action
            .clone()
            .unwrap_or_else(|| "-".to_string())
    ));

    for (i, line) in lines.iter().enumerate() {
        let mut text = line.clone();
        if text.len() > width {
            text.truncate(width);
        } else {
            text.push_str(&" ".repeat(width - text.len()));
        }
        term.render(&[
            Change::CursorPosition {
                x: Position::Absolute(start_x),
                y: Position::Absolute(i),
            },
            Change::Text(text),
        ])?;
    }
    term.render(&[Change::CursorPosition {
        x: Position::Absolute(0),
        y: Position::Absolute(screen.rows.saturating_sub(1)),
    }])?;
    Ok(())
}

#[derive(serde::Deserialize)]
struct PlanResponse {
    summary: String,
    mermaid: Option<String>,
    steps: Vec<String>,
}

fn request_plan(config: &ConfigHandle, task: &str) -> anyhow::Result<PlanResponse> {
    let prompt = format!(
        "Create a concise plan and a Mermaid diagram. Return JSON: \
{{\"summary\":\"...\",\"mermaid\":\"...\",\"steps\":[\"...\"]}}.\nTask: {task}"
    );
    let response = request_text(config, &prompt)?;
    let response = strip_json_fences(&response);
    serde_json::from_str(&response).context("invalid plan response JSON")
}

#[derive(serde::Deserialize)]
struct ActionResponse {
    kind: String,
    content: String,
    command: String,
}

fn request_action(config: &ConfigHandle, task: &str) -> anyhow::Result<ActionResponse> {
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

fn prompt_bool(
    term: &mut TermWizTerminal,
    host: &mut CodeHost,
    prompt: &str,
    default: bool,
) -> anyhow::Result<bool> {
    let default_label = if default { "y" } else { "n" };
    let mut editor = LineEditor::new(term);
    editor.set_prompt(&format!("{prompt} [{default_label}]: "));
    let line = editor.read_line(host)?;
    let line = line.unwrap_or_default();
    let line = line.trim();
    if line.is_empty() {
        return Ok(default);
    }
    Ok(matches!(line, "y" | "Y" | "yes" | "YES"))
}

fn execute_command(command: &str) -> anyhow::Result<String> {
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
        combined = "(no output)".to_string();
    }
    Ok(combined)
}
