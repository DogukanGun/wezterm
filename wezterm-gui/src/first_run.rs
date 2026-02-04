use anyhow::{anyhow, Context};
use config::{AiProvider, DefaultPaneMode, CONFIG_DIRS, HOME_DIR};
use keyring::Entry;
use mux::termwiztermtab::TermWizTerminal;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use termwiz::lineedit::{BasicHistory, History, LineEditor, LineEditorHost};
use termwiz::surface::Change;
use termwiz::terminal::Terminal;
use crate::ollama;
use wezterm_term::TerminalConfiguration;

const KEYRING_SERVICE: &str = "wezterm";

pub async fn maybe_run_first_run_wizard(
    skip_config: bool,
    config_file: Option<std::ffi::OsString>,
) -> anyhow::Result<()> {
    log::info!("first-run wizard: enter");
    if skip_config || config_file.is_some() || config::is_config_overridden() {
        log::info!("first-run wizard: skipped (config overrides)");
        return Ok(());
    }

    if config_file_exists() {
        log::info!("first-run wizard: skipped (config file exists)");
        return Ok(());
    }

    log::info!("first-run wizard: launching");
    let config = config::configuration();
    let dpi = config.dpi.unwrap_or_else(|| ::window::default_dpi());
    let size = config.initial_size(dpi as u32, Some(crate::cell_pixel_dims(&config, dpi)?));
    let term_config: Arc<dyn TerminalConfiguration + Send + Sync> =
        Arc::new(config::TermConfig::with_config(config.clone()));

    std::thread::spawn(move || {
        let result = promise::spawn::block_on(mux::termwiztermtab::run(
            size,
            None,
            run_wizard,
            Some(term_config),
        ));
        if let Err(err) = result {
            log::error!("first-run wizard failed: {err:#}");
            return;
        }
        config::reload();
        log::info!("first-run wizard: completed");
    });

    Ok(())
}

fn config_file_exists() -> bool {
    let mut paths = vec![HOME_DIR.join(".wezterm.lua")];
    for dir in CONFIG_DIRS.iter() {
        paths.push(dir.join("wezterm.lua"));
    }
    if let Some(path) = std::env::var_os("WEZTERM_CONFIG_FILE") {
        paths.insert(0, PathBuf::from(path));
    }
    paths.iter().any(|path| path.exists())
}

fn run_wizard(mut term: TermWizTerminal) -> anyhow::Result<()> {
    term.no_grab_mouse_in_raw_mode();
    term.render(&[
        Change::Text("Welcome to WezTerm setup.\r\n".to_string()),
        Change::Text("Press Enter to accept defaults.\r\n\r\n".to_string()),
    ])?;

    let mut host = WizardHost::new();
    let enable_ai = prompt_bool(&mut term, &mut host, "Enable AI module? (y/N)", false)?;

    let default_pane_mode = DefaultPaneMode::Terminal;
    let mut ai_provider = AiProvider::Ollama;
    let mut ai_model = None;
    let mut ollama_base_url = "http://localhost:11434/v1".to_string();

    if enable_ai {
        let provider = prompt_choice(
            &mut term,
            &mut host,
            "AI provider (ollama/openai/anthropic)",
            &["ollama", "openai", "anthropic"],
            "ollama",
        )?;
        ai_provider = match provider.as_str() {
            "openai" => AiProvider::OpenAI,
            "anthropic" => AiProvider::Anthropic,
            _ => AiProvider::Ollama,
        };

        let model = prompt_string(&mut term, &mut host, "AI model name", None)?;
        if !model.is_empty() {
            ai_model = Some(model);
        } else if matches!(ai_provider, AiProvider::Ollama) {
            ai_model = Some("qwen2.5-coder:7b".to_string());
        }

        if matches!(ai_provider, AiProvider::Ollama) {
            let url = prompt_string(
                &mut term,
                &mut host,
                "Ollama base URL",
                Some(&ollama_base_url),
            )?;
            if !url.is_empty() {
                ollama_base_url = url;
            }

            let _ = ollama::ensure_ollama_ready(
                &ollama_base_url,
                ai_model.as_deref(),
                |message| {
                let _ = term.render(&[Change::Text(format!("{message}\r\n"))]);
                },
            );
        }

        if matches!(ai_provider, AiProvider::OpenAI | AiProvider::Anthropic) {
            let key_label = if matches!(ai_provider, AiProvider::OpenAI) {
                "OpenAI"
            } else {
                "Anthropic"
            };
            let key = prompt_string(
                &mut term,
                &mut host,
                &format!("{key_label} API key (leave blank to skip)"),
                None,
            )?;
            if !key.is_empty() {
                store_api_key(&ai_provider, &key)?;
            }
        }
    }

    let code_mode_enabled = true;
    let code_mode_auto_accept = prompt_bool(
        &mut term,
        &mut host,
        "Code Mode auto-accept commands? (y/N)",
        false,
    )?;

    let solana_rpc_url = prompt_string(
        &mut term,
        &mut host,
        "Solana RPC URL (optional)",
        None,
    )?;

    write_config(
        enable_ai,
        default_pane_mode,
        ai_provider,
        ai_model,
        &ollama_base_url,
        if solana_rpc_url.is_empty() {
            None
        } else {
            Some(solana_rpc_url)
        },
        code_mode_enabled,
        code_mode_auto_accept,
    )?;

    term.render(&[Change::Text(
        "\r\nSetup complete. Restarting with new settings.\r\n".to_string(),
    )])?;

    Ok(())
}

fn write_config(
    enable_ai: bool,
    default_pane_mode: DefaultPaneMode,
    ai_provider: AiProvider,
    ai_model: Option<String>,
    ollama_base_url: &str,
    solana_rpc_url: Option<String>,
    code_mode_enabled: bool,
    code_mode_auto_accept: bool,
) -> anyhow::Result<()> {
    let config_path = HOME_DIR.join(".wezterm.lua");
    let mut lines = Vec::new();
    lines.push("local wezterm = require 'wezterm'".to_string());
    lines.push("return {".to_string());
    lines.push(format!("  enable_ai_module = {},", if enable_ai { "true" } else { "false" }));

    if enable_ai {
        let pane_mode = match default_pane_mode {
            DefaultPaneMode::Ai => "Ai",
            DefaultPaneMode::Terminal => "Terminal",
        };
        lines.push(format!("  default_pane_mode = \"{pane_mode}\","));

        let provider = match ai_provider {
            AiProvider::Ollama => "Ollama",
            AiProvider::OpenAI => "OpenAI",
            AiProvider::Anthropic => "Anthropic",
        };
        lines.push(format!("  ai_provider = \"{provider}\","));
        if let Some(model) = ai_model {
            lines.push(format!("  ai_model = \"{}\",", model));
        }
        if matches!(ai_provider, AiProvider::Ollama) {
            lines.push(format!("  ai_ollama_base_url = \"{}\",", ollama_base_url));
        }
    }

    if let Some(url) = solana_rpc_url {
        lines.push(format!("  solana_rpc_url = \"{}\",", url));
    }

    lines.push("  color_scheme = \"SolanaFuturistic\",".to_string());
    lines.push("  use_fancy_tab_bar = true,".to_string());
    lines.push("  window_frame = {".to_string());
    lines.push("    active_titlebar_bg = \"#0b0f1a\",".to_string());
    lines.push("    inactive_titlebar_bg = \"#0b0f1a\",".to_string());
    lines.push("    active_titlebar_fg = \"#e6f1ff\",".to_string());
    lines.push("    inactive_titlebar_fg = \"#8aa0c8\",".to_string());
    lines.push("    active_titlebar_border_bottom = \"#1e2a3a\",".to_string());
    lines.push("    inactive_titlebar_border_bottom = \"#1e2a3a\",".to_string());
    lines.push("    button_fg = \"#e6f1ff\",".to_string());
    lines.push("    button_bg = \"#0b0f1a\",".to_string());
    lines.push("    button_hover_fg = \"#14f195\",".to_string());
    lines.push("    button_hover_bg = \"#1e2a3a\",".to_string());
    lines.push("  },".to_string());
    lines.push("  colors = {".to_string());
    lines.push("    tab_bar = {".to_string());
    lines.push("      background = \"#0b0f1a\",".to_string());
    lines.push("      active_tab = { bg_color = \"#141a2b\", fg_color = \"#e6f1ff\", intensity = \"Bold\" },".to_string());
    lines.push("      inactive_tab = { bg_color = \"#0b0f1a\", fg_color = \"#8aa0c8\" },".to_string());
    lines.push("      inactive_tab_hover = { bg_color = \"#141a2b\", fg_color = \"#e6f1ff\" },".to_string());
    lines.push("      new_tab = { bg_color = \"#0b0f1a\", fg_color = \"#14f195\" },".to_string());
    lines.push("      new_tab_hover = { bg_color = \"#141a2b\", fg_color = \"#14f195\" },".to_string());
    lines.push("      inactive_tab_edge = \"#1e2a3a\",".to_string());
    lines.push("      inactive_tab_edge_hover = \"#1e2a3a\",".to_string());
    lines.push("    },".to_string());
    lines.push("  },".to_string());

    lines.push(format!(
        "  code_mode_enabled = {},",
        if code_mode_enabled { "true" } else { "false" }
    ));
    lines.push(format!(
        "  code_mode_auto_accept = {},",
        if code_mode_auto_accept { "true" } else { "false" }
    ));

    lines.push("}".to_string());
    let content = lines.join("\n") + "\n";

    fs::write(&config_path, content)
        .with_context(|| format!("writing config to {}", config_path.display()))?;
    Ok(())
}

fn store_api_key(provider: &AiProvider, key: &str) -> anyhow::Result<()> {
    let account = match provider {
        AiProvider::OpenAI => "openai",
        AiProvider::Anthropic => "anthropic",
        AiProvider::Ollama => return Ok(()),
    };
    let entry = Entry::new(KEYRING_SERVICE, account)?;
    entry.set_password(key)?;
    Ok(())
}

struct WizardHost {
    history: BasicHistory,
}

impl WizardHost {
    fn new() -> Self {
        Self {
            history: BasicHistory::default(),
        }
    }
}

impl LineEditorHost for WizardHost {
    fn history(&mut self) -> &mut dyn History {
        &mut self.history
    }
}

fn prompt_string(
    term: &mut TermWizTerminal,
    host: &mut WizardHost,
    prompt: &str,
    default: Option<&str>,
) -> anyhow::Result<String> {
    let mut editor = LineEditor::new(term);
    let label = if let Some(value) = default {
        format!("{prompt} [{value}]: ")
    } else {
        format!("{prompt}: ")
    };
    editor.set_prompt(&label);
    let line = editor.read_line(host)?;
    let line = line.unwrap_or_default();
    let line = line.trim().to_string();
    if line.is_empty() {
        Ok(default.unwrap_or("").to_string())
    } else {
        Ok(line)
    }
}

fn prompt_bool(
    term: &mut TermWizTerminal,
    host: &mut WizardHost,
    prompt: &str,
    default: bool,
) -> anyhow::Result<bool> {
    let default_label = if default { "y" } else { "n" };
    let answer = prompt_string(term, host, prompt, Some(default_label))?;
    if answer.is_empty() {
        return Ok(default);
    }
    Ok(matches!(answer.as_str(), "y" | "Y" | "yes" | "YES"))
}

fn prompt_choice(
    term: &mut TermWizTerminal,
    host: &mut WizardHost,
    prompt: &str,
    choices: &[&str],
    default: &str,
) -> anyhow::Result<String> {
    let answer = prompt_string(term, host, prompt, Some(default))?;
    let normalized = answer.to_lowercase();
    if choices.contains(&normalized.as_str()) {
        Ok(normalized)
    } else if answer.is_empty() {
        Ok(default.to_string())
    } else {
        Err(anyhow!("invalid choice: {}", answer))
    }
}
