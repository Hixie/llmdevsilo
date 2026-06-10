/// Options for spawning a local harness and their mapping to a `silo run`
/// argument list. Pure Dart with no platform dependencies, so the argument
/// construction is unit-testable and usable from the web stub's signatures.
library;

/// LLM backends offered when spawning a local harness, with the `--llm`
/// flag value and the per-backend defaults for the model and the API key
/// environment variable. An empty default means the flag is omitted.
enum LlmBackendChoice {
  anthropic('anthropic', 'claude-sonnet-4-6', 'ANTHROPIC_API_KEY'),
  openai('openai', 'gpt-5', 'OPENAI_API_KEY'),
  openaiWs('openai-ws', 'gpt-4o-realtime-preview', 'OPENAI_API_KEY'),
  local('local', '', '');

  const LlmBackendChoice(
    this.cliName,
    this.defaultModel,
    this.defaultApiKeyEnv,
  );

  /// Value for the `--llm` flag.
  final String cliName;

  /// Default model name; empty omits the `--model` flag.
  final String defaultModel;

  /// Conventional environment variable holding the backend's API key;
  /// empty when the backend takes no key.
  final String defaultApiKeyEnv;
}

/// Sandbox backends offered when spawning a local harness.
enum SandboxChoice {
  auto('auto'),
  mock('mock');

  const SandboxChoice(this.cliName);

  /// Value for the `--sandbox` flag.
  final String cliName;
}

/// Everything needed to compose one `silo run` invocation.
class LocalHarnessOptions {
  const LocalHarnessOptions({
    required this.workspaceDir,
    this.createWorkspace = true,
    this.backend = LlmBackendChoice.anthropic,
    this.model = '',
    this.apiKeyEnv = '',
    this.sandbox = SandboxChoice.auto,
    this.allowedDomains = const [],
    this.readAllowlist = const [],
    this.quotaUsd,
  });

  /// Workspace directory passed to `--workspace`.
  final String workspaceDir;

  /// Passes `--create` so the workspace is locked before the session.
  final bool createWorkspace;

  final LlmBackendChoice backend;

  /// Model name; empty omits the `--model` flag (the harness default
  /// applies).
  final String model;

  /// Environment variable holding the API key; empty omits the
  /// `--api-key-env` flag.
  final String apiKeyEnv;

  final SandboxChoice sandbox;

  /// Domains the sandbox may reach (`--allow-domain`, repeated).
  final List<String> allowedDomains;

  /// Host paths the sandbox may read (`--allow-read`, repeated).
  final List<String> readAllowlist;

  /// Session dollar quota (`--quota-usd`); null omits the flag.
  final double? quotaUsd;
}

/// The argument list for the `silo` binary built from [options].
List<String> buildRunArgs(LocalHarnessOptions options) => [
      'run',
      '--workspace',
      options.workspaceDir,
      if (options.createWorkspace) '--create',
      '--llm',
      options.backend.cliName,
      if (options.model.isNotEmpty) ...['--model', options.model],
      if (options.apiKeyEnv.isNotEmpty) ...[
        '--api-key-env',
        options.apiKeyEnv,
      ],
      '--sandbox',
      options.sandbox.cliName,
      for (final domain in options.allowedDomains) ...[
        '--allow-domain',
        domain,
      ],
      for (final path in options.readAllowlist) ...['--allow-read', path],
      if (options.quotaUsd != null) ...[
        '--quota-usd',
        options.quotaUsd.toString(),
      ],
    ];

/// The full command line for [options], shell-quoted for display and for
/// copy-pasting into a terminal.
String runCommandLine(LocalHarnessOptions options) =>
    ['silo', ...buildRunArgs(options)].map(shellQuote).join(' ');

final RegExp _shellSafe = RegExp(r'^[A-Za-z0-9_\-./:=@%+,]+$');

/// Quotes [value] for a POSIX shell. Values made only of unquoted-safe
/// characters pass through; everything else is single-quoted, with embedded
/// single quotes escaped.
String shellQuote(String value) {
  if (value.isEmpty) {
    return "''";
  }
  if (_shellSafe.hasMatch(value)) {
    return value;
  }
  return "'${value.replaceAll("'", "'\\''")}'";
}

/// Splits a one-entry-per-line text field into trimmed, non-empty lines.
List<String> splitLines(String text) => text
    .split('\n')
    .map((line) => line.trim())
    .where((line) => line.isNotEmpty)
    .toList();

/// A local harness failed to start: the spawned process exited before its
/// run file appeared, or could not be spawned at all. [stderrTail] holds
/// the last lines of the process's standard error, when available.
class HarnessStartError implements Exception {
  HarnessStartError(this.message, {this.stderrTail = ''});

  final String message;
  final String stderrTail;

  @override
  String toString() =>
      stderrTail.isEmpty ? message : '$message\n$stderrTail';
}
