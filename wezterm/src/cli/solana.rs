use anyhow::{anyhow, Context};
use bip39::{Language, Mnemonic};
use clap::Parser;
use config;
use ed25519_dalek::{SigningKey, VerifyingKey};
use keyring::Entry;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const KEYRING_SERVICE: &str = "wezterm-solana";

#[derive(Debug, Parser, Clone)]
pub struct SolanaCommand {
    #[command(subcommand)]
    sub: SolanaSubCommand,
}

#[derive(Debug, Parser, Clone)]
enum SolanaSubCommand {
    #[command(name = "create-wallet", about = "Create a new Solana wallet")]
    CreateWallet(CreateWallet),

    #[command(name = "import-wallet", about = "Import a wallet from a mnemonic")]
    ImportWallet(ImportWallet),

    #[command(name = "address", about = "Show the wallet address")]
    Address(Address),

    #[command(name = "show-mnemonic", about = "Show the stored mnemonic")]
    ShowMnemonic(ShowMnemonic),

    #[command(name = "agent", about = "Run the Solana AI agent")]
    Agent(Agent),
}

#[derive(Debug, Parser, Clone)]
struct CreateWallet {
    /// Optional wallet profile name
    #[arg(long, default_value = "default")]
    profile: String,

    /// Print the generated mnemonic to stdout
    #[arg(long = "show-mnemonic")]
    show_mnemonic: bool,
}

#[derive(Debug, Parser, Clone)]
struct ImportWallet {
    /// Optional wallet profile name
    #[arg(long, default_value = "default")]
    profile: String,

    /// Mnemonic words (use --mnemonic-file for safer input)
    #[arg(long)]
    mnemonic: Option<String>,

    /// Read mnemonic from a file path
    #[arg(long = "mnemonic-file")]
    mnemonic_file: Option<PathBuf>,
}

#[derive(Debug, Parser, Clone)]
struct Address {
    /// Optional wallet profile name
    #[arg(long, default_value = "default")]
    profile: String,
}

#[derive(Debug, Parser, Clone)]
struct ShowMnemonic {
    /// Optional wallet profile name
    #[arg(long, default_value = "default")]
    profile: String,
}

#[derive(Debug, Parser, Clone)]
struct Agent {
    /// Optional wallet profile name
    #[arg(long, default_value = "default")]
    profile: String,

    /// Optional override path to the agent.mjs script
    #[arg(long = "script-path")]
    script_path: Option<PathBuf>,
}

impl SolanaCommand {
    pub async fn run(self) -> anyhow::Result<()> {
        match self.sub {
            SolanaSubCommand::CreateWallet(cmd) => cmd.run(),
            SolanaSubCommand::ImportWallet(cmd) => cmd.run(),
            SolanaSubCommand::Address(cmd) => cmd.run(),
            SolanaSubCommand::ShowMnemonic(cmd) => cmd.run(),
            SolanaSubCommand::Agent(cmd) => cmd.run(),
        }
    }
}

impl CreateWallet {
    fn run(self) -> anyhow::Result<()> {
        ensure_profile_unused(&self.profile)?;
        let mnemonic = Mnemonic::generate_in(Language::English, 24)?;
        let keypair = keypair_from_mnemonic(&mnemonic)?;
        store_wallet(&self.profile, &mnemonic, &keypair)?;
        let address = bs58::encode(keypair.verifying_key().to_bytes()).into_string();
        println!("Address: {address}");
        if self.show_mnemonic {
            println!("Mnemonic: {}", mnemonic.to_string());
        } else {
            println!("Mnemonic: (hidden) use --show-mnemonic to display");
        }
        Ok(())
    }
}

impl ImportWallet {
    fn run(self) -> anyhow::Result<()> {
        let mnemonic = load_mnemonic_from_args(self.mnemonic, self.mnemonic_file)?;
        ensure_profile_unused(&self.profile)?;
        let keypair = keypair_from_mnemonic(&mnemonic)?;
        store_wallet(&self.profile, &mnemonic, &keypair)?;
        let address = bs58::encode(keypair.verifying_key().to_bytes()).into_string();
        println!("Address: {address}");
        Ok(())
    }
}

impl Address {
    fn run(self) -> anyhow::Result<()> {
        let keypair = load_keypair(&self.profile)?;
        let address = bs58::encode(keypair.verifying_key().to_bytes()).into_string();
        println!("{address}");
        Ok(())
    }
}

impl ShowMnemonic {
    fn run(self) -> anyhow::Result<()> {
        let mnemonic = load_mnemonic(&self.profile)?;
        println!("{mnemonic}");
        Ok(())
    }
}

impl Agent {
    fn run(self) -> anyhow::Result<()> {
        let secret_b58 = load_keypair_secret(&self.profile)?;
        let script_path = resolve_agent_script(self.script_path)?;
        let config = config::configuration();

        let mut cmd = Command::new("node");
        cmd.arg(script_path);
        cmd.env("WEZTERM_SOLANA_SECRET_KEY", secret_b58);
        if let Some(url) = config.solana_rpc_url.clone() {
            cmd.env("WEZTERM_SOLANA_RPC_URL", url);
        }
        cmd.env("WEZTERM_AI_PROVIDER", ai_provider_name(&config.ai_provider));
        if let Some(model) = config.ai_model.clone() {
            cmd.env("WEZTERM_AI_MODEL", model);
        }
        cmd.env("WEZTERM_OLLAMA_BASE_URL", config.ai_ollama_base_url.clone());

        if let Some(key) = load_ai_key("openai", "WEZTERM_OPENAI_API_KEY")? {
            cmd.env("WEZTERM_OPENAI_API_KEY", key);
        }
        if let Some(key) = load_ai_key("anthropic", "WEZTERM_ANTHROPIC_API_KEY")? {
            cmd.env("WEZTERM_ANTHROPIC_API_KEY", key);
        }

        cmd.stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());

        let status = cmd.status().context("failed to run solana agent")?;
        if !status.success() {
            return Err(anyhow!("solana agent exited with {}", status));
        }
        Ok(())
    }
}

fn load_mnemonic_from_args(
    mnemonic: Option<String>,
    mnemonic_file: Option<PathBuf>,
) -> anyhow::Result<Mnemonic> {
    let phrase = if let Some(path) = mnemonic_file {
        fs::read_to_string(&path)
            .with_context(|| format!("reading mnemonic file {}", path.display()))?
    } else if let Some(value) = mnemonic {
        value
    } else {
        return Err(anyhow!("provide --mnemonic or --mnemonic-file"));
    };
    let phrase = phrase.trim();
    Ok(Mnemonic::parse_in(Language::English, phrase)?)
}

fn keypair_from_mnemonic(mnemonic: &Mnemonic) -> anyhow::Result<SigningKey> {
    let seed_bytes = mnemonic.to_seed("");
    let mut key_seed = [0u8; 32];
    key_seed.copy_from_slice(&seed_bytes[..32]);
    Ok(SigningKey::from_bytes(&key_seed))
}

fn store_wallet(profile: &str, mnemonic: &Mnemonic, keypair: &SigningKey) -> anyhow::Result<()> {
    let mnemonic_entry = Entry::new(KEYRING_SERVICE, &format!("mnemonic:{profile}"))?;
    mnemonic_entry.set_password(&mnemonic.to_string())?;

    let mut combined = [0u8; 64];
    combined[..32].copy_from_slice(&keypair.to_bytes());
    combined[32..].copy_from_slice(&keypair.verifying_key().to_bytes());
    let secret_b58 = bs58::encode(combined).into_string();

    let keypair_entry = Entry::new(KEYRING_SERVICE, &format!("keypair:{profile}"))?;
    keypair_entry.set_password(&secret_b58)?;

    Ok(())
}

fn load_mnemonic(profile: &str) -> anyhow::Result<String> {
    let mnemonic_entry = Entry::new(KEYRING_SERVICE, &format!("mnemonic:{profile}"))?;
    let phrase = mnemonic_entry
        .get_password()
        .context("mnemonic not found; run wezterm solana create-wallet")?;
    Ok(phrase)
}

fn load_keypair(profile: &str) -> anyhow::Result<SigningKey> {
    let keypair_entry = Entry::new(KEYRING_SERVICE, &format!("keypair:{profile}"))?;
    let secret_b58 = keypair_entry
        .get_password()
        .context("wallet not found; run wezterm solana create-wallet")?;
    let decoded = bs58::decode(secret_b58)
        .into_vec()
        .context("invalid keypair data")?;
    if decoded.len() != 64 {
        return Err(anyhow!("invalid keypair length"));
    }
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&decoded[..32]);
    Ok(SigningKey::from_bytes(&secret))
}

fn load_keypair_secret(profile: &str) -> anyhow::Result<String> {
    let keypair_entry = Entry::new(KEYRING_SERVICE, &format!("keypair:{profile}"))?;
    keypair_entry
        .get_password()
        .context("wallet not found; run wezterm solana create-wallet")
}

fn ensure_profile_unused(profile: &str) -> anyhow::Result<()> {
    let keypair_entry = Entry::new(KEYRING_SERVICE, &format!("keypair:{profile}"))?;
    if keypair_entry.get_password().is_ok() {
        return Err(anyhow!("wallet profile already exists"));
    }
    Ok(())
}

fn resolve_agent_script(script_path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = script_path {
        return Ok(path);
    }
    if let Ok(path) = std::env::var("WEZTERM_SOLANA_AGENT_JS") {
        return Ok(PathBuf::from(path));
    }
    let default_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("solana-agent")
        .join("agent.mjs");
    if default_path.exists() {
        return Ok(default_path);
    }
    Err(anyhow!(
        "Solana agent script not found. Set WEZTERM_SOLANA_AGENT_JS or --script-path"
    ))
}

fn load_ai_key(account: &str, env_key: &str) -> anyhow::Result<Option<String>> {
    if let Ok(entry) = Entry::new("wezterm", account) {
        if let Ok(value) = entry.get_password() {
            return Ok(Some(value));
        }
    }
    Ok(std::env::var(env_key).ok())
}

fn ai_provider_name(provider: &config::AiProvider) -> String {
    match provider {
        config::AiProvider::Ollama => "ollama",
        config::AiProvider::OpenAI => "openai",
        config::AiProvider::Anthropic => "anthropic",
    }
    .to_string()
}

trait SigningKeyExt {
    fn verifying_key(&self) -> VerifyingKey;
}

impl SigningKeyExt for SigningKey {
    fn verifying_key(&self) -> VerifyingKey {
        VerifyingKey::from(self)
    }
}
