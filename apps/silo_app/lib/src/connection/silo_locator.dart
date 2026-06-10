/// Locates the `silo` binary on the host. Pure Dart: callers supply the
/// environment and a file-exists predicate, so the candidate ordering is
/// unit-testable without touching the filesystem.
library;

/// Guidance shown when no `silo` binary can be found.
const String siloNotFoundMessage =
    'silo was not found. Build it with "cargo build --release" in the '
    'llmdevsilo repository and enter the path to target/release/silo here, '
    'or install it on PATH.';

/// The first existing `silo` binary among, in order: [configuredPath], the
/// `SILO_BIN` environment variable, each entry of `PATH` joined with
/// `silo`, `target/release/silo` and `target/debug/silo` under each
/// ancestor directory of [executablePath] (so an app running out of the
/// llmdevsilo repository finds the workspace build), then
/// `$HOME/.cargo/bin/silo`, `/opt/homebrew/bin/silo`, and
/// `/usr/local/bin/silo`. Candidates that are empty or for which
/// [fileExists] returns false are skipped. Returns null when no candidate
/// exists. Paths use POSIX conventions (`:`-separated `PATH`, `/` joins);
/// harness spawning is only offered on macOS.
String? locateSilo({
  String? configuredPath,
  required Map<String, String> environment,
  required bool Function(String path) fileExists,
  String? executablePath,
}) {
  for (final candidate in siloCandidates(
    configuredPath: configuredPath,
    environment: environment,
    executablePath: executablePath,
  )) {
    if (fileExists(candidate)) {
      return candidate;
    }
  }
  return null;
}

/// The candidate paths probed by [locateSilo], in probe order, with empty
/// entries dropped.
List<String> siloCandidates({
  String? configuredPath,
  required Map<String, String> environment,
  String? executablePath,
}) {
  final home = environment['HOME'] ?? '';
  return [
    ?configuredPath,
    environment['SILO_BIN'] ?? '',
    for (final dir in (environment['PATH'] ?? '').split(':'))
      if (dir.isNotEmpty) '$dir/silo',
    for (final ancestor in _ancestors(executablePath)) ...[
      '$ancestor/target/release/silo',
      '$ancestor/target/debug/silo',
    ],
    if (home.isNotEmpty) '$home/.cargo/bin/silo',
    '/opt/homebrew/bin/silo',
    '/usr/local/bin/silo',
  ].where((path) => path.isNotEmpty).toList();
}

/// Ancestor directories of an absolute path, nearest first, excluding the
/// filesystem root. For the development app bundle (which lives under
/// `<repo>/apps/silo_app/build/...`) the repository root is among these,
/// which is how a plain `cargo build` is discovered.
List<String> _ancestors(String? path) {
  if (path == null || !path.startsWith('/')) {
    return const [];
  }
  final parts = path.split('/').where((part) => part.isNotEmpty).toList();
  final ancestors = <String>[];
  // Start at the parent directory of the path itself.
  for (var length = parts.length - 1; length > 0; length--) {
    ancestors.add('/${parts.sublist(0, length).join('/')}');
  }
  return ancestors;
}
