import readline from "node:readline";
import { Keypair } from "@solana/web3.js";
import bs58 from "bs58";
import {
  SolanaAgentKit,
  KeypairWallet,
} from "solana-agent-kit";
import TokenPlugin from "@solana-agent-kit/plugin-token";
import NFTPlugin from "@solana-agent-kit/plugin-nft";
import DefiPlugin from "@solana-agent-kit/plugin-defi";
import MiscPlugin from "@solana-agent-kit/plugin-misc";
import BlinksPlugin from "@solana-agent-kit/plugin-blinks";

const secretKey = process.env.WEZTERM_SOLANA_SECRET_KEY;
if (!secretKey) {
  console.error("Missing WEZTERM_SOLANA_SECRET_KEY");
  process.exit(1);
}

const rpcUrl = process.env.WEZTERM_SOLANA_RPC_URL;
const provider = (process.env.WEZTERM_AI_PROVIDER || "ollama").toLowerCase();
const model = process.env.WEZTERM_AI_MODEL;
const ollamaBaseUrl =
  process.env.WEZTERM_OLLAMA_BASE_URL || "http://localhost:11434/v1";
const openaiBaseUrl = "https://api.openai.com/v1";
const openaiKey = process.env.WEZTERM_OPENAI_API_KEY || "";
const anthropicKey = process.env.WEZTERM_ANTHROPIC_API_KEY || "";

const keypair = Keypair.fromSecretKey(bs58.decode(secretKey));
const wallet = new KeypairWallet(keypair);

const agent = new SolanaAgentKit(wallet, rpcUrl, {
  OPENAI_API_KEY: openaiKey,
  OPENAI_BASE_URL: ollamaBaseUrl,
  ANTHROPIC_API_KEY: anthropicKey,
})
  .use(TokenPlugin)
  .use(NFTPlugin)
  .use(DefiPlugin)
  .use(MiscPlugin)
  .use(BlinksPlugin);

const methodNames = Object.keys(agent.methods || {}).sort();

function stripJsonFences(text) {
  const trimmed = text.trim();
  if (trimmed.startsWith("```")) {
    return trimmed.replace(/```[a-zA-Z]*\n?/g, "").replace(/```$/, "").trim();
  }
  return trimmed;
}

async function callOpenAICompatible(prompt) {
  const baseUrl = provider === "openai" ? openaiBaseUrl : ollamaBaseUrl;
  const response = await fetch(`${baseUrl}/chat/completions`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      ...(provider === "openai" ? { authorization: `Bearer ${openaiKey}` } : {}),
    },
    body: JSON.stringify({
      model,
      messages: [
        { role: "system", content: systemPrompt() },
        { role: "user", content: prompt },
      ],
    }),
  });

  if (!response.ok) {
    const text = await response.text();
    throw new Error(`LLM request failed: ${text}`);
  }
  const data = await response.json();
  return data.choices?.[0]?.message?.content || "";
}

async function callAnthropic(prompt) {
  const response = await fetch("https://api.anthropic.com/v1/messages", {
    method: "POST",
    headers: {
      "content-type": "application/json",
      "x-api-key": anthropicKey,
      "anthropic-version": "2023-06-01",
    },
    body: JSON.stringify({
      model,
      max_tokens: 1024,
      messages: [{ role: "user", content: prompt }],
      system: systemPrompt(),
    }),
  });

  if (!response.ok) {
    const text = await response.text();
    throw new Error(`LLM request failed: ${text}`);
  }
  const data = await response.json();
  return data.content?.[0]?.text || "";
}

function systemPrompt() {
  return [
    "You are a Solana command agent.",
    "Return JSON only: {\"method\":\"<name>\",\"args\":[...]}",
    "Valid method names:",
    methodNames.join(", "),
  ].join("\n");
}

async function decideAction(prompt) {
  if (!model) {
    throw new Error("WEZTERM_AI_MODEL is required for Solana AI");
  }
  if (provider === "anthropic") {
    return callAnthropic(prompt);
  }
  return callOpenAICompatible(prompt);
}

async function executeAction(payload) {
  const method = payload.method;
  const args = Array.isArray(payload.args) ? payload.args : [];
  const fn = agent.methods?.[method];
  if (!fn) {
    throw new Error(`Unknown method: ${method}`);
  }
  return fn(agent, ...args);
}

const rl = readline.createInterface({
  input: process.stdin,
  output: process.stdout,
});

console.log("Solana AI agent ready. Type /exit to quit.");
rl.setPrompt("solana> ");
rl.prompt();

rl.on("line", async (line) => {
  const trimmed = line.trim();
  if (!trimmed) {
    rl.prompt();
    return;
  }
  if (trimmed === "/exit") {
    rl.close();
    return;
  }
  try {
    let payloadText = trimmed;
    if (!trimmed.startsWith("{")) {
      const responseText = await decideAction(trimmed);
      payloadText = stripJsonFences(responseText);
    }
    const payload = JSON.parse(payloadText);
    const result = await executeAction(payload);
    console.log(JSON.stringify(result, null, 2));
  } catch (err) {
    console.error(`Error: ${err.message || err}`);
  }
  rl.prompt();
});

rl.on("close", () => {
  process.exit(0);
});
