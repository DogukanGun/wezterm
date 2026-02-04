use anyhow::{anyhow, Context};
use reqwest::blocking::Client;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const DOCKER_CONTAINER: &str = "wezterm-ollama";
const DOCKER_IMAGE: &str = "ollama/ollama";
const DEFAULT_PULL_MODEL: &str = "qwen2.5-coder:7b";

pub fn ensure_ollama_ready<F>(base_url: &str, model: Option<&str>, mut status: F) -> anyhow::Result<()>
where
    F: FnMut(&str),
{
    let base_url = base_url.trim_end_matches('/');
    let model = model.unwrap_or(DEFAULT_PULL_MODEL);

    status("Checking Ollama availability...");
    if check_openai_models(base_url)? {
        pull_model_if_possible(model, &mut status)?;
        return Ok(());
    }

    if is_local_base_url(base_url) && ollama_cli_available() {
        status("Starting Ollama service...");
        start_ollama_serve()?;
        wait_for_models(base_url, &mut status)?;
        pull_model_if_possible(model, &mut status)?;
        return Ok(());
    }

    if is_local_base_url(base_url) {
        status("Starting Ollama via Docker...");
        ensure_docker_available()?;
        ensure_docker_container()?;
        wait_for_models(base_url, &mut status)?;
        pull_model_via_docker(model, &mut status)?;
        return Ok(());
    }

    Err(anyhow!(
        "Ollama is not available at {base_url}. Ensure Ollama is running or update ai_ollama_base_url."
    ))
}

fn check_openai_models(base_url: &str) -> anyhow::Result<bool> {
    let url = format!("{base_url}/models");
    let client = Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .context("building HTTP client")?;
    let response = client.get(url).send();
    match response {
        Ok(resp) => Ok(resp.status().is_success()),
        Err(_) => Ok(false),
    }
}

fn wait_for_models<F>(base_url: &str, status: &mut F) -> anyhow::Result<()>
where
    F: FnMut(&str),
{
    let mut delay = Duration::from_millis(250);
    for _ in 0..16 {
        if check_openai_models(base_url)? {
            status("Ollama is ready.");
            return Ok(());
        }
        status("Waiting for Ollama to start...");
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(4));
    }
    Err(anyhow!(
        "Ollama did not become ready at {base_url}. Please check the service."
    ))
}

fn is_local_base_url(base_url: &str) -> bool {
    base_url.contains("localhost")
        || base_url.contains("127.0.0.1")
        || base_url.contains("0.0.0.0")
}

fn ollama_cli_available() -> bool {
    Command::new("ollama")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn start_ollama_serve() -> anyhow::Result<()> {
    Command::new("ollama")
        .arg("serve")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting ollama serve")?;
    Ok(())
}

fn pull_model_if_possible<F>(model: &str, status: &mut F) -> anyhow::Result<()>
where
    F: FnMut(&str),
{
    if !ollama_cli_available() {
        status("Ollama CLI not available; skipping model pull.");
        return Ok(());
    }
    status(&format!("Pulling model {model} with Ollama..."));
    let output = Command::new("ollama")
        .arg("pull")
        .arg(model)
        .output()
        .context("running ollama pull")?;
    if !output.status.success() {
        return Err(anyhow!(
            "ollama pull failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

fn ensure_docker_available() -> anyhow::Result<()> {
    let status = Command::new("docker")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("checking docker availability")?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("Docker is not available or not running"))
    }
}

fn ensure_docker_container() -> anyhow::Result<()> {
    if docker_container_running()? {
        return Ok(());
    }
    if docker_container_exists()? {
        Command::new("docker")
            .arg("start")
            .arg(DOCKER_CONTAINER)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("starting Ollama docker container")?;
        return Ok(());
    }
    Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            DOCKER_CONTAINER,
            "-p",
            "11434:11434",
            DOCKER_IMAGE,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("running Ollama docker container")?;
    Ok(())
}

fn docker_container_exists() -> anyhow::Result<bool> {
    let output = Command::new("docker")
        .args(["ps", "-a", "--filter", &format!("name={DOCKER_CONTAINER}"), "--format", "{{.Names}}"])
        .output()
        .context("checking docker container existence")?;
    if !output.status.success() {
        return Ok(false);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().any(|line| line.trim() == DOCKER_CONTAINER))
}

fn docker_container_running() -> anyhow::Result<bool> {
    let output = Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", DOCKER_CONTAINER])
        .output()
        .context("checking docker container state")?;
    if !output.status.success() {
        return Ok(false);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim() == "true")
}

fn pull_model_via_docker<F>(model: &str, status: &mut F) -> anyhow::Result<()>
where
    F: FnMut(&str),
{
    status(&format!("Pulling model {model} in Docker..."));
    let output = Command::new("docker")
        .args(["exec", DOCKER_CONTAINER, "ollama", "pull", model])
        .output()
        .context("pulling model in docker")?;
    if !output.status.success() {
        return Err(anyhow!(
            "docker ollama pull failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}
