//! Command-line definitions and the mapping from flags to the harness
//! configuration.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use clap::{Args, Parser, Subcommand, ValueEnum};

use silo_core::config::{
    FrontendKind, HarnessConfig, LlmBackendKind, LlmConfig, SandboxConfig, SandboxKind,
};
use silo_core::secrets::CredentialInjection;

#[derive(Parser, Debug)]
#[command(
    name = "silo",
    version,
    about = "Sandboxed LLM coding harness",
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run one harness session.
    Run(Box<RunArgs>),
    /// Manage workspace locks.
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// Open an interactive shell under the same sandbox policy as the LLM.
    ///
    /// The workspace may already be attached to a running harness; the
    /// shell then shares the workspace mount, so the harness's work is
    /// visible live. When no sandbox flags are given and a harness is
    /// running, the shell mirrors that harness's sandbox kind, read
    /// allowlist, and allowed domains. Credential injection is never
    /// mirrored; only credentials given with --inject-credential apply.
    Shell(ShellArgs),
    /// Convert a journal into a replayable test script.
    ReplayTest(ReplayTestArgs),
    /// Inspect running harnesses.
    Harnesses {
        #[command(subcommand)]
        action: HarnessesAction,
    },
    /// Write man pages generated from the command definitions.
    #[command(hide = true)]
    Manpages {
        /// Directory the pages are written into (created if needed).
        output_dir: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
pub enum WorkspaceAction {
    /// Lock a directory as a workspace (creating it if needed).
    Lock { path: PathBuf },
    /// Unlock a workspace and report every change since locking.
    Unlock { path: PathBuf },
    /// Show the lock and attach state of a workspace.
    Status { path: PathBuf },
}

#[derive(Subcommand, Debug)]
pub enum HarnessesAction {
    /// List live harnesses and prune dead run files.
    List,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum FrontendOpt {
    Interactive,
    Headless,
    Mock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum LlmOpt {
    Anthropic,
    Openai,
    OpenaiWs,
    Local,
    Mock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SandboxOpt {
    Auto,
    Mock,
    SandboxExec,
    LinuxVm,
    Gvisor,
    Microvm,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Locked workspace directory (required unless --config provides one).
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    /// Lock the workspace first (it must be new, empty, or unlocked).
    #[arg(long)]
    pub create: bool,
    #[arg(long, value_enum)]
    pub frontend: Option<FrontendOpt>,
    /// Initial prompt (required by the headless frontend).
    #[arg(long)]
    pub prompt: Option<String>,
    /// Test script JSON (required by mock components).
    #[arg(long)]
    pub script: Option<PathBuf>,
    /// LLM backend. Defaults by environment: OPENAI_API_KEY selects
    /// openai, else ANTHROPIC_API_KEY selects anthropic.
    #[arg(long, value_enum)]
    pub llm: Option<LlmOpt>,
    #[arg(long)]
    pub model: Option<String>,
    /// Environment variable holding the LLM API key.
    #[arg(long)]
    pub api_key_env: Option<String>,
    #[arg(long)]
    pub base_url: Option<String>,
    /// Sandbox backend. Defaults to auto: sandbox-exec on macOS, gvisor
    /// on Linux. Mock is only used when selected explicitly.
    #[arg(long, value_enum)]
    pub sandbox: Option<SandboxOpt>,
    /// Listen address for the interactive WebSocket server.
    #[arg(long)]
    pub listen: Option<SocketAddr>,
    /// PEM certificate chain for the interactive server (requires
    /// --tls-key).
    #[arg(long = "tls-cert")]
    pub tls_cert: Option<PathBuf>,
    /// PEM private key matching --tls-cert.
    #[arg(long = "tls-key")]
    pub tls_key: Option<PathBuf>,
    /// Host path the sandbox may read (repeatable).
    #[arg(long = "allow-read")]
    pub allow_read: Vec<PathBuf>,
    /// Domain the sandbox may reach (repeatable; "*.example.com" matches
    /// example.com and every subdomain).
    #[arg(long = "allow-domain")]
    pub allow_domain: Vec<String>,
    /// Credential injection: host:header:ENV_VAR[:format] (repeatable).
    #[arg(long = "inject-credential")]
    pub inject_credential: Vec<String>,
    /// Session token quota.
    #[arg(long)]
    pub quota_tokens: Option<u64>,
    /// Session dollar quota.
    #[arg(long)]
    pub quota_usd: Option<f64>,
    /// Journal file path.
    #[arg(long)]
    pub journal: Option<PathBuf>,
    /// Fake clock; byte-stable journals.
    #[arg(long)]
    pub deterministic: bool,
    /// Use the mock proxy with any sandbox backend.
    #[arg(long)]
    pub mock_proxy: bool,
    /// Print a one-time pairing code at startup (interactive frontend).
    #[arg(long)]
    pub pairing_code: bool,
    /// Accept this read-allowlist entry despite risk-scan hits
    /// (repeatable).
    #[arg(long = "allow-risky-path")]
    pub allow_risky_path: Vec<PathBuf>,
    /// TOML configuration file; flags override its values.
    #[arg(long)]
    pub config: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ShellArgs {
    /// Locked workspace directory.
    #[arg(long)]
    pub workspace: PathBuf,
    /// Host path the sandbox may read (repeatable). Overrides mirroring.
    #[arg(long = "allow-read")]
    pub allow_read: Vec<PathBuf>,
    /// Domain the sandbox may reach (repeatable; "*.example.com" matches
    /// example.com and every subdomain). Overrides mirroring.
    #[arg(long = "allow-domain")]
    pub allow_domain: Vec<String>,
    /// Sandbox backend. Defaults to the running harness's backend when
    /// mirroring, otherwise to auto. Overrides mirroring. Mock is
    /// rejected: it is script-driven and only usable via silo run.
    #[arg(long, value_enum)]
    pub sandbox: Option<SandboxOpt>,
    /// Credential injection: host:header:ENV_VAR[:format] (repeatable).
    /// Credentials are never mirrored from a running harness; only the
    /// ones given here are injected.
    #[arg(long = "inject-credential")]
    pub inject_credential: Vec<String>,
    /// Accept this read-allowlist entry despite risk-scan hits
    /// (repeatable).
    #[arg(long = "allow-risky-path")]
    pub allow_risky_path: Vec<PathBuf>,
    /// Command to run instead of an interactive shell.
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Args, Debug)]
pub struct ReplayTestArgs {
    /// Journal file (JSON Lines) recorded by a harness session.
    pub journal: PathBuf,
    /// Output path for the generated test script.
    #[arg(short, long)]
    pub output: PathBuf,
    /// Script name; defaults to the journal file stem.
    #[arg(long)]
    pub name: Option<String>,
}

/// Resolves the "auto" sandbox choice for the current platform:
/// sandbox-exec on macOS, gVisor on Linux.
pub fn resolve_sandbox_kind(opt: SandboxOpt) -> SandboxKind {
    match opt {
        SandboxOpt::Auto => {
            if cfg!(target_os = "macos") {
                SandboxKind::MacosSandboxExec
            } else {
                SandboxKind::LinuxGvisor
            }
        }
        SandboxOpt::Mock => SandboxKind::Mock,
        SandboxOpt::SandboxExec => SandboxKind::MacosSandboxExec,
        SandboxOpt::LinuxVm => SandboxKind::MacosLinuxVm,
        SandboxOpt::Gvisor => SandboxKind::LinuxGvisor,
        SandboxOpt::Microvm => SandboxKind::LinuxMicrovm,
    }
}

/// Maps a sandbox kind name, as reported in a run file (the
/// `Sandbox::kind` string), back to the configuration enum. Unknown names
/// yield `None`.
pub fn sandbox_kind_from_name(name: &str) -> Option<SandboxKind> {
    match name {
        "mock" => Some(SandboxKind::Mock),
        "macos-sandbox-exec" => Some(SandboxKind::MacosSandboxExec),
        "macos-linux-vm" => Some(SandboxKind::MacosLinuxVm),
        "linux-gvisor" => Some(SandboxKind::LinuxGvisor),
        "linux-microvm" => Some(SandboxKind::LinuxMicrovm),
        _ => None,
    }
}

fn resolve_llm_kind(opt: LlmOpt) -> LlmBackendKind {
    match opt {
        LlmOpt::Anthropic => LlmBackendKind::Anthropic,
        LlmOpt::Openai => LlmBackendKind::OpenaiResponses,
        LlmOpt::OpenaiWs => LlmBackendKind::OpenaiWebsocket,
        LlmOpt::Local => LlmBackendKind::Local,
        LlmOpt::Mock => LlmBackendKind::Mock,
    }
}

fn resolve_frontend_kind(opt: FrontendOpt) -> FrontendKind {
    match opt {
        FrontendOpt::Interactive => FrontendKind::Interactive,
        FrontendOpt::Headless => FrontendKind::Headless,
        FrontendOpt::Mock => FrontendKind::Mock,
    }
}

/// Parses one --inject-credential value: `host:header:ENV_VAR[:format]`.
/// The format part may contain colons.
pub fn parse_inject_credential(value: &str) -> anyhow::Result<CredentialInjection> {
    let mut parts = value.splitn(4, ':');
    let host = parts.next().unwrap_or_default();
    let header = parts.next().unwrap_or_default();
    let value_env = parts.next().unwrap_or_default();
    let format = parts.next().unwrap_or("{secret}");
    if host.is_empty() || header.is_empty() || value_env.is_empty() {
        bail!("invalid --inject-credential {value:?}: expected host:header:ENV_VAR[:format]");
    }
    Ok(CredentialInjection {
        host: host.to_string(),
        header: header.to_string(),
        value_env: value_env.to_string(),
        format: format.to_string(),
    })
}

/// An LLM backend chosen from the environment when neither `--llm` nor a
/// configuration-file backend selects one.
#[derive(Clone, Debug, PartialEq)]
pub struct EnvLlmDefault {
    pub backend: LlmBackendKind,
    pub model: &'static str,
    pub api_key_env: &'static str,
}

/// Resolves the default LLM backend from an environment map. A variable
/// set to the empty string counts as unset. `OPENAI_API_KEY` selects the
/// OpenAI Responses backend with its default model; otherwise
/// `ANTHROPIC_API_KEY` selects the Anthropic backend.
pub fn default_llm_from_env(env: &HashMap<String, String>) -> Option<EnvLlmDefault> {
    let is_set = |name: &str| env.get(name).is_some_and(|value| !value.is_empty());
    if is_set("OPENAI_API_KEY") {
        return Some(EnvLlmDefault {
            backend: LlmBackendKind::OpenaiResponses,
            model: "gpt-5",
            api_key_env: "OPENAI_API_KEY",
        });
    }
    if is_set("ANTHROPIC_API_KEY") {
        return Some(EnvLlmDefault {
            backend: LlmBackendKind::Anthropic,
            model: "claude-sonnet-4-6",
            api_key_env: "ANTHROPIC_API_KEY",
        });
    }
    None
}

/// Validates `silo shell` arguments beyond what clap expresses.
pub fn validate_shell_args(args: &ShellArgs) -> anyhow::Result<()> {
    if args.sandbox == Some(SandboxOpt::Mock) {
        bail!(
            "--sandbox mock is not usable with silo shell: the mock sandbox \
             is script-driven and only usable via silo run --script"
        );
    }
    Ok(())
}

/// Warnings about a resolved run configuration, printed to standard error
/// by `silo run` before the session starts.
pub fn startup_warnings(config: &HarnessConfig) -> Vec<String> {
    let mut warnings = Vec::new();
    let allowlist =
        silo_proxy::allowlist::DomainAllowlist::new(&config.sandbox.proxy.allowed_domains);
    for credential in &config.sandbox.proxy.credentials {
        if !allowlist.allows(&credential.host) {
            warnings.push(format!(
                "the credential for {host} can never apply: no allowed domain \
                 covers {host}; add --allow-domain {host}",
                host = credential.host
            ));
        }
    }
    let paid = matches!(
        config.llm.backend,
        LlmBackendKind::Anthropic
            | LlmBackendKind::OpenaiResponses
            | LlmBackendKind::OpenaiWebsocket
    );
    if paid && config.llm.quota.max_total_tokens.is_none() && config.llm.quota.max_usd.is_none() {
        warnings.push(
            "no session quota is set for a paid LLM backend; consider \
             --quota-tokens or --quota-usd"
                .into(),
        );
    }
    warnings
}

/// Configuration-file keys that take precedence over environment-based
/// and platform-based defaults when present.
#[derive(Default)]
struct ConfigFileKeys {
    llm_backend: bool,
    sandbox_kind: bool,
}

fn config_file_keys(path: &Path) -> anyhow::Result<ConfigFileKeys> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let value: toml::Value =
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
    let has = |table: &str, key: &str| value.get(table).and_then(|t| t.get(key)).is_some();
    Ok(ConfigFileKeys {
        llm_backend: has("llm", "backend"),
        sandbox_kind: has("sandbox", "kind"),
    })
}

/// Builds the harness configuration for `silo run`: the TOML file (when
/// given) supplies the base and every flag overrides it. Auto-selection
/// notes are printed to standard error.
pub fn build_run_config(args: &RunArgs) -> anyhow::Result<HarnessConfig> {
    let env: HashMap<String, String> = std::env::vars().collect();
    let (config, notes) = build_run_config_from(args, &env)?;
    for note in &notes {
        eprintln!("{note}");
    }
    Ok(config)
}

fn build_run_config_from(
    args: &RunArgs,
    env: &HashMap<String, String>,
) -> anyhow::Result<(HarnessConfig, Vec<String>)> {
    let mut file_keys = ConfigFileKeys::default();
    let mut config = match &args.config {
        Some(path) => {
            file_keys = config_file_keys(path)?;
            HarnessConfig::load(path)
                .with_context(|| format!("loading config {}", path.display()))?
        }
        None => {
            let workspace = args
                .workspace
                .clone()
                .context("either --workspace or --config is required")?;
            HarnessConfig {
                harness_id: silo_core::short_id(),
                workspace,
                llm: LlmConfig::default(),
                sandbox: SandboxConfig::default(),
                frontend: silo_core::config::FrontendConfig::default(),
                logging: silo_core::config::LoggingConfig::default(),
            }
        }
    };

    if let Some(workspace) = &args.workspace {
        config.workspace = workspace.clone();
    }
    if let Some(frontend) = args.frontend {
        config.frontend.kind = resolve_frontend_kind(frontend);
    }
    if let Some(prompt) = &args.prompt {
        config.frontend.headless_prompt = Some(prompt.clone());
    }
    if let Some(listen) = args.listen {
        config.frontend.listen_addr = Some(listen);
    }
    if let Some(tls_cert) = &args.tls_cert {
        config.frontend.tls_cert_path = Some(tls_cert.clone());
    }
    if let Some(tls_key) = &args.tls_key {
        config.frontend.tls_key_path = Some(tls_key.clone());
    }
    if args.pairing_code {
        config.frontend.issue_pairing_code = true;
    }
    let mut notes = Vec::new();
    if let Some(llm) = args.llm {
        config.llm.backend = resolve_llm_kind(llm);
    } else if !file_keys.llm_backend {
        let Some(chosen) = default_llm_from_env(env) else {
            bail!(
                "no LLM backend selected: pass --llm, or set OPENAI_API_KEY \
                 or ANTHROPIC_API_KEY in the environment"
            );
        };
        let name = match chosen.backend {
            LlmBackendKind::OpenaiResponses => "openai",
            _ => "anthropic",
        };
        notes.push(format!(
            "auto-selected the {name} LLM backend (model {}) because {} is \
             set; pass --llm or --model to override",
            chosen.model, chosen.api_key_env
        ));
        config.llm.backend = chosen.backend;
        config.llm.model = chosen.model.to_string();
        config.llm.api_key_env = Some(chosen.api_key_env.to_string());
    }
    if let Some(model) = &args.model {
        config.llm.model = model.clone();
    }
    if let Some(api_key_env) = &args.api_key_env {
        config.llm.api_key_env = Some(api_key_env.clone());
    }
    if let Some(base_url) = &args.base_url {
        config.llm.base_url = Some(base_url.clone());
    }
    if let Some(quota_tokens) = args.quota_tokens {
        config.llm.quota.max_total_tokens = Some(quota_tokens);
    }
    if let Some(quota_usd) = args.quota_usd {
        config.llm.quota.max_usd = Some(quota_usd);
    }
    if let Some(sandbox) = args.sandbox {
        config.sandbox.kind = resolve_sandbox_kind(sandbox);
    } else if !file_keys.sandbox_kind {
        config.sandbox.kind = resolve_sandbox_kind(SandboxOpt::Auto);
    }
    if !args.allow_read.is_empty() {
        config.sandbox.read_allowlist = args.allow_read.clone();
    }
    if !args.allow_domain.is_empty() {
        config.sandbox.proxy.allowed_domains = args.allow_domain.clone();
    }
    if !args.inject_credential.is_empty() {
        config.sandbox.proxy.credentials = args
            .inject_credential
            .iter()
            .map(|value| parse_inject_credential(value))
            .collect::<anyhow::Result<Vec<_>>>()?;
    }
    if let Some(journal) = &args.journal {
        config.logging.journal_path = Some(journal.clone());
    }

    // Cross-field requirements.
    if config.frontend.kind == FrontendKind::Headless && config.frontend.headless_prompt.is_none() {
        bail!("the headless frontend requires --prompt");
    }
    if config.frontend.tls_cert_path.is_some() != config.frontend.tls_key_path.is_some() {
        bail!("--tls-cert and --tls-key (or tls_cert_path and tls_key_path) must be set together");
    }
    let needs_script = config.frontend.kind == FrontendKind::Mock
        || config.llm.backend == LlmBackendKind::Mock
        || config.sandbox.kind == SandboxKind::Mock;
    if needs_script && args.script.is_none() {
        bail!("mock components (frontend, llm, or sandbox) require --script");
    }

    Ok((config, notes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_run(extra: &[&str]) -> RunArgs {
        let mut argv = vec!["silo", "run", "--workspace", "/tmp/ws"];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv).expect("args parse").command {
            Command::Run(args) => *args,
            other => panic!("expected run, got {other:?}"),
        }
    }

    #[test]
    fn inject_credential_parses_with_and_without_format() {
        let plain = parse_inject_credential("api.github.com:Authorization:GITHUB_TOKEN").unwrap();
        assert_eq!(plain.host, "api.github.com");
        assert_eq!(plain.header, "Authorization");
        assert_eq!(plain.value_env, "GITHUB_TOKEN");
        assert_eq!(plain.format, "{secret}");

        let formatted =
            parse_inject_credential("api.github.com:Authorization:GITHUB_TOKEN:Bearer {secret}:v2")
                .unwrap();
        assert_eq!(formatted.format, "Bearer {secret}:v2");

        assert!(parse_inject_credential("host:header").is_err());
        assert!(parse_inject_credential("::ENV").is_err());
    }

    #[test]
    fn inject_credential_flag_lands_in_the_proxy_settings() {
        let args = parse_run(&[
            "--llm",
            "anthropic",
            "--sandbox",
            "gvisor",
            "--inject-credential",
            "api.github.com:Authorization:GH_TOKEN:token {secret}",
        ]);
        let config = build_run_config(&args).unwrap();
        assert_eq!(config.sandbox.proxy.credentials.len(), 1);
        let cred = &config.sandbox.proxy.credentials[0];
        assert_eq!(cred.host, "api.github.com");
        assert_eq!(cred.format, "token {secret}");
    }

    #[test]
    fn sandbox_auto_selects_the_platform_backend() {
        let expected = if cfg!(target_os = "macos") {
            SandboxKind::MacosSandboxExec
        } else {
            SandboxKind::LinuxGvisor
        };
        assert_eq!(resolve_sandbox_kind(SandboxOpt::Auto), expected);
        assert_eq!(resolve_sandbox_kind(SandboxOpt::Mock), SandboxKind::Mock);
        assert_eq!(
            resolve_sandbox_kind(SandboxOpt::Microvm),
            SandboxKind::LinuxMicrovm
        );

        let args = parse_run(&["--llm", "anthropic", "--sandbox", "auto"]);
        let config = build_run_config(&args).unwrap();
        assert_eq!(config.sandbox.kind, expected);
    }

    #[test]
    fn sandbox_kind_names_map_back_to_kinds() {
        assert_eq!(sandbox_kind_from_name("mock"), Some(SandboxKind::Mock));
        assert_eq!(
            sandbox_kind_from_name("macos-sandbox-exec"),
            Some(SandboxKind::MacosSandboxExec)
        );
        assert_eq!(
            sandbox_kind_from_name("linux-gvisor"),
            Some(SandboxKind::LinuxGvisor)
        );
        assert_eq!(sandbox_kind_from_name("unheard-of"), None);
    }

    #[test]
    fn quota_flags_map_to_the_llm_quota() {
        let args = parse_run(&[
            "--llm",
            "anthropic",
            "--sandbox",
            "gvisor",
            "--quota-tokens",
            "50000",
            "--quota-usd",
            "2.5",
        ]);
        let config = build_run_config(&args).unwrap();
        assert_eq!(config.llm.quota.max_total_tokens, Some(50_000));
        assert_eq!(config.llm.quota.max_usd, Some(2.5));
    }

    #[test]
    fn llm_and_frontend_flags_map_to_kinds() {
        let args = parse_run(&[
            "--llm",
            "openai-ws",
            "--sandbox",
            "gvisor",
            "--frontend",
            "headless",
            "--prompt",
            "do it",
            "--model",
            "gpt-test",
        ]);
        let config = build_run_config(&args).unwrap();
        assert_eq!(config.llm.backend, LlmBackendKind::OpenaiWebsocket);
        assert_eq!(config.llm.model, "gpt-test");
        assert_eq!(config.frontend.kind, FrontendKind::Headless);
        assert_eq!(config.frontend.headless_prompt.as_deref(), Some("do it"));
    }

    #[test]
    fn tls_flags_map_to_the_frontend_config() {
        let args = parse_run(&[
            "--llm",
            "anthropic",
            "--sandbox",
            "gvisor",
            "--tls-cert",
            "/tmp/cert.pem",
            "--tls-key",
            "/tmp/key.pem",
        ]);
        let config = build_run_config(&args).unwrap();
        assert_eq!(
            config.frontend.tls_cert_path,
            Some(PathBuf::from("/tmp/cert.pem"))
        );
        assert_eq!(
            config.frontend.tls_key_path,
            Some(PathBuf::from("/tmp/key.pem"))
        );
    }

    #[test]
    fn tls_cert_and_key_must_be_set_together() {
        for flags in [
            &["--tls-cert", "/tmp/cert.pem"][..],
            &["--tls-key", "/tmp/key.pem"][..],
        ] {
            let mut argv = vec!["--llm", "anthropic", "--sandbox", "gvisor"];
            argv.extend_from_slice(flags);
            let args = parse_run(&argv);
            let error = build_run_config(&args).unwrap_err();
            assert!(error.to_string().contains("--tls-cert"), "{flags:?}");
        }
    }

    #[test]
    fn headless_requires_a_prompt() {
        let args = parse_run(&[
            "--llm",
            "anthropic",
            "--sandbox",
            "gvisor",
            "--frontend",
            "headless",
        ]);
        let error = build_run_config(&args).unwrap_err();
        assert!(error.to_string().contains("--prompt"));
    }

    #[test]
    fn mock_components_require_a_script() {
        for flags in [
            &["--llm", "mock"][..],
            &[
                "--frontend",
                "mock",
                "--llm",
                "anthropic",
                "--sandbox",
                "gvisor",
            ][..],
            &["--sandbox", "mock", "--llm", "anthropic"][..],
        ] {
            let args = parse_run(flags);
            let error = build_run_config(&args).unwrap_err();
            assert!(error.to_string().contains("--script"), "{flags:?}");
        }
    }

    #[test]
    fn workspace_is_required_without_a_config_file() {
        let argv = vec!["silo", "run", "--llm", "anthropic"];
        let Command::Run(args) = Cli::try_parse_from(argv).unwrap().command else {
            panic!("expected run");
        };
        assert!(build_run_config(&args).is_err());
    }

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn env_default_prefers_openai_over_anthropic() {
        let both = env_map(&[("OPENAI_API_KEY", "sk-o"), ("ANTHROPIC_API_KEY", "sk-a")]);
        let chosen = default_llm_from_env(&both).unwrap();
        assert_eq!(chosen.backend, LlmBackendKind::OpenaiResponses);
        assert_eq!(chosen.model, "gpt-5");
        assert_eq!(chosen.api_key_env, "OPENAI_API_KEY");
    }

    #[test]
    fn env_default_falls_back_to_anthropic() {
        let only_anthropic = env_map(&[("ANTHROPIC_API_KEY", "sk-a")]);
        let chosen = default_llm_from_env(&only_anthropic).unwrap();
        assert_eq!(chosen.backend, LlmBackendKind::Anthropic);
        assert_eq!(chosen.model, "claude-sonnet-4-6");
        assert_eq!(chosen.api_key_env, "ANTHROPIC_API_KEY");
    }

    #[test]
    fn empty_env_values_count_as_unset() {
        let empty_openai = env_map(&[("OPENAI_API_KEY", ""), ("ANTHROPIC_API_KEY", "sk-a")]);
        assert_eq!(
            default_llm_from_env(&empty_openai).unwrap().backend,
            LlmBackendKind::Anthropic
        );
        let all_empty = env_map(&[("OPENAI_API_KEY", ""), ("ANTHROPIC_API_KEY", "")]);
        assert!(default_llm_from_env(&all_empty).is_none());
    }

    #[test]
    fn no_backend_anywhere_is_an_error_naming_the_options() {
        let args = parse_run(&[]);
        let error = build_run_config_from(&args, &HashMap::new()).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("--llm"), "{message}");
        assert!(message.contains("OPENAI_API_KEY"), "{message}");
        assert!(message.contains("ANTHROPIC_API_KEY"), "{message}");
    }

    #[test]
    fn env_resolution_sets_backend_model_and_key_env_with_a_note() {
        let args = parse_run(&[]);
        let env = env_map(&[("OPENAI_API_KEY", "sk-o")]);
        let (config, notes) = build_run_config_from(&args, &env).unwrap();
        assert_eq!(config.llm.backend, LlmBackendKind::OpenaiResponses);
        assert_eq!(config.llm.model, "gpt-5");
        assert_eq!(config.llm.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("OPENAI_API_KEY"), "{}", notes[0]);
        assert!(notes[0].contains("openai"), "{}", notes[0]);
    }

    #[test]
    fn model_and_key_env_flags_override_the_env_defaults() {
        let args = parse_run(&["--model", "gpt-custom", "--api-key-env", "MY_KEY"]);
        let env = env_map(&[("OPENAI_API_KEY", "sk-o")]);
        let (config, _) = build_run_config_from(&args, &env).unwrap();
        assert_eq!(config.llm.backend, LlmBackendKind::OpenaiResponses);
        assert_eq!(config.llm.model, "gpt-custom");
        assert_eq!(config.llm.api_key_env.as_deref(), Some("MY_KEY"));
    }

    #[test]
    fn explicit_llm_flag_wins_over_the_environment() {
        let args = parse_run(&["--llm", "anthropic"]);
        let env = env_map(&[("OPENAI_API_KEY", "sk-o")]);
        let (config, notes) = build_run_config_from(&args, &env).unwrap();
        assert_eq!(config.llm.backend, LlmBackendKind::Anthropic);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn config_file_backend_wins_over_env_sniffing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("run.toml");
        std::fs::write(
            &path,
            "workspace = \"/tmp/ws\"\n\n[llm]\nbackend = \"local\"\n",
        )
        .unwrap();
        let argv = vec!["silo", "run", "--config", path.to_str().unwrap()];
        let Command::Run(args) = Cli::try_parse_from(argv).unwrap().command else {
            panic!("expected run");
        };
        let env = env_map(&[("OPENAI_API_KEY", "sk-o")]);
        let (config, notes) = build_run_config_from(&args, &env).unwrap();
        assert_eq!(config.llm.backend, LlmBackendKind::Local);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn sandbox_defaults_to_auto_without_a_flag_or_config_kind() {
        let expected = if cfg!(target_os = "macos") {
            SandboxKind::MacosSandboxExec
        } else {
            SandboxKind::LinuxGvisor
        };
        let args = parse_run(&["--llm", "anthropic"]);
        let (config, _) = build_run_config_from(&args, &HashMap::new()).unwrap();
        assert_eq!(config.sandbox.kind, expected);
    }

    #[test]
    fn config_file_sandbox_kind_wins_over_the_auto_default() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("run.toml");
        std::fs::write(
            &path,
            "workspace = \"/tmp/ws\"\n\n[sandbox]\nkind = \"linux_microvm\"\n",
        )
        .unwrap();
        let argv = vec![
            "silo",
            "run",
            "--config",
            path.to_str().unwrap(),
            "--llm",
            "anthropic",
        ];
        let Command::Run(args) = Cli::try_parse_from(argv).unwrap().command else {
            panic!("expected run");
        };
        let (config, _) = build_run_config_from(&args, &HashMap::new()).unwrap();
        assert_eq!(config.sandbox.kind, SandboxKind::LinuxMicrovm);
    }

    #[test]
    fn shell_rejects_the_mock_sandbox() {
        let argv = vec!["silo", "shell", "--workspace", "/tmp/ws"];
        let Command::Shell(args) = Cli::try_parse_from(argv).unwrap().command else {
            panic!("expected shell");
        };
        assert!(validate_shell_args(&args).is_ok());

        let argv = vec![
            "silo",
            "shell",
            "--workspace",
            "/tmp/ws",
            "--sandbox",
            "mock",
        ];
        let Command::Shell(args) = Cli::try_parse_from(argv).unwrap().command else {
            panic!("expected shell");
        };
        let error = validate_shell_args(&args).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("script-driven"), "{message}");
        assert!(message.contains("silo run --script"), "{message}");
    }

    #[test]
    fn startup_warnings_flag_uncovered_credentials_and_missing_quotas() {
        let args = parse_run(&[
            "--llm",
            "anthropic",
            "--sandbox",
            "gvisor",
            "--allow-domain",
            "*.example.com",
            "--inject-credential",
            "api.github.com:Authorization:GH_TOKEN",
        ]);
        let (config, _) = build_run_config_from(&args, &HashMap::new()).unwrap();
        let warnings = startup_warnings(&config);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("api.github.com"), "{}", warnings[0]);
        assert!(warnings[0].contains("--allow-domain"), "{}", warnings[0]);
        assert!(warnings[1].contains("--quota-tokens"), "{}", warnings[1]);
        assert!(warnings[1].contains("--quota-usd"), "{}", warnings[1]);
    }

    #[test]
    fn startup_warnings_are_silent_when_covered_and_quotad() {
        let args = parse_run(&[
            "--llm",
            "anthropic",
            "--sandbox",
            "gvisor",
            "--allow-domain",
            "*.github.com",
            "--inject-credential",
            "api.github.com:Authorization:GH_TOKEN",
            "--quota-usd",
            "5",
        ]);
        let (config, _) = build_run_config_from(&args, &HashMap::new()).unwrap();
        assert!(startup_warnings(&config).is_empty());

        // The mock backend is free; no quota warning applies.
        let args = parse_run(&["--llm", "mock", "--script", "/tmp/script.json"]);
        let (config, _) = build_run_config_from(&args, &HashMap::new()).unwrap();
        assert!(startup_warnings(&config).is_empty());
    }
}
