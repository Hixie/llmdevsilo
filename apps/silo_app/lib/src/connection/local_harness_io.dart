/// Local harness discovery and spawning, for platforms with `dart:io`.
///
/// Local harnesses advertise themselves with run files
/// (`~/.llmdevsilo/run/<harness_id>.json`, override with
/// `LLMDEVSILO_STATE_DIR`) containing a `RunInfo`. Reading the run file and
/// the local-token file it points to gives everything needed to connect.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:io';

import '../protocol/protocol.dart';
import 'local_harness_options.dart';
import 'silo_locator.dart';

/// True when run-file discovery is available (desktop platforms).
bool get localRunsSupported =>
    Platform.isMacOS || Platform.isLinux || Platform.isWindows;

/// True when the app can spawn a new local harness (macOS desktop).
bool get canSpawnHarness => Platform.isMacOS;

/// The first existing `silo` binary, probing [configuredPath], `SILO_BIN`,
/// `PATH`, workspace `target/` builds above the running app bundle, and
/// the conventional install locations. Null when none exists.
String? resolveSiloBinary(String? configuredPath) => locateSilo(
      configuredPath: configuredPath,
      environment: Platform.environment,
      fileExists: siloBinaryExists,
      executablePath: Platform.resolvedExecutable,
    );

/// True when a file exists at [path].
bool siloBinaryExists(String path) => File(path).existsSync();

String stateDir() {
  final override = Platform.environment['LLMDEVSILO_STATE_DIR'];
  if (override != null && override.isNotEmpty) {
    return override;
  }
  final home = Platform.environment['HOME'] ??
      Platform.environment['USERPROFILE'] ??
      '.';
  return '$home/.llmdevsilo';
}

/// Probe for whether the process with a given pid is running.
typedef PidProbe = Future<bool> Function(int pid);

/// Runs an executable and returns its result; injectable for tests.
typedef RunProcess = Future<ProcessResult> Function(
    String executable, List<String> arguments);

/// Sends signal 0 to [pid] (`kill -0`), which tests for existence without
/// delivering anything. A failure whose stderr indicates a permission
/// error counts as alive: the process exists but belongs to another user.
/// Returns true on platforms without `kill` and on probe errors, so an
/// unprobeable run file is shown rather than hidden.
Future<bool> pidIsAlive(int pid, {RunProcess? runProcess}) async {
  if (!Platform.isMacOS && !Platform.isLinux) {
    return true;
  }
  final run = runProcess ?? Process.run;
  try {
    final result = await run('kill', ['-0', '$pid']);
    if (result.exitCode == 0) {
      return true;
    }
    final stderr = '${result.stderr}'.toLowerCase();
    return stderr.contains('not permitted') || stderr.contains('eperm');
  } catch (_) {
    return true;
  }
}

/// Run files of currently live local harnesses. Run files whose pid is no
/// longer running are skipped but never deleted: the silo CLI owns
/// pruning. [isAlive] defaults to [pidIsAlive]; [runDir] defaults to the
/// `run` directory under [stateDir].
Future<List<RunInfo>> listLocalRuns({
  PidProbe? isAlive,
  String? runDir,
}) async {
  final probe = isAlive ?? pidIsAlive;
  final dir = Directory(runDir ?? '${stateDir()}/run');
  if (!await dir.exists()) {
    return [];
  }
  final runs = <RunInfo>[];
  await for (final entry in dir.list()) {
    if (entry is! File || !entry.path.endsWith('.json')) {
      continue;
    }
    try {
      final json =
          jsonDecode(await entry.readAsString()) as Map<String, dynamic>;
      final run = RunInfo.fromJson(json);
      if (await probe(run.pid)) {
        runs.add(run);
      }
    } catch (_) {
      // A run file may be mid-write or stale; skip it.
    }
  }
  runs.sort((a, b) => a.harnessId.compareTo(b.harnessId));
  return runs;
}

/// Reads the local auth token the run file points to.
Future<String> readLocalToken(RunInfo info) async {
  return (await File(info.localTokenPath).readAsString()).trim();
}

/// True when the workspace registry
/// (`<state>/workspaces/registry.json`) lists [dir], by its canonical
/// path, as a locked workspace. Returns false when the directory or the
/// registry cannot be read.
Future<bool> isWorkspaceLocked(String dir) async {
  try {
    final canonical = await Directory(dir).resolveSymbolicLinks();
    final file = File('${stateDir()}/workspaces/registry.json');
    if (!await file.exists()) {
      return false;
    }
    final registry =
        jsonDecode(await file.readAsString()) as Map<String, dynamic>;
    final entry = registry[canonical];
    return entry is Map<String, dynamic> && entry['locked'] == true;
  } catch (_) {
    return false;
  }
}

const _stderrTailLines = 40;

/// Starts `silo run` with the binary and arguments from [options] and
/// waits for its run file to appear. The process is detached so it outlives
/// the app, with stdio pipes kept so startup errors are readable. Returns
/// the new harness's `RunInfo`, or null when the process is still running
/// at [timeout]. Throws [HarnessStartError] when the binary is missing,
/// the process cannot be spawned, or it exits before its run file appears,
/// with the stderr tail.
Future<RunInfo?> startLocalHarness(
  LocalHarnessOptions options, {
  Duration timeout = const Duration(seconds: 30),
}) async {
  if (!siloBinaryExists(options.siloBinary)) {
    throw HarnessStartError(
      'There is no file at "${options.siloBinary}". $siloNotFoundMessage',
    );
  }
  final before = {for (final r in await listLocalRuns()) r.harnessId};
  final Process process;
  try {
    process = await Process.start(
      options.siloBinary,
      buildRunArgs(options),
      mode: ProcessStartMode.detachedWithStdio,
    );
  } on ProcessException catch (error) {
    throw HarnessStartError(
      'Could not start "${options.siloBinary}": ${error.message}. '
      '$siloNotFoundMessage',
    );
  }
  // Detached processes expose no exit code; the stderr stream closing
  // signals that the process has exited. Keep the last lines for error
  // reporting.
  final stderrLines = <String>[];
  var exited = false;
  unawaited(process.stdout.drain<void>().then((_) {}, onError: (_) {}));
  unawaited(process.stderr
      .transform(utf8.decoder)
      .transform(const LineSplitter())
      .forEach((line) {
        stderrLines.add(line);
        if (stderrLines.length > _stderrTailLines) {
          stderrLines.removeAt(0);
        }
      })
      .then((_) {}, onError: (_) {})
      .whenComplete(() => exited = true));
  final deadline = DateTime.now().add(timeout);
  while (DateTime.now().isBefore(deadline)) {
    await Future<void>.delayed(const Duration(milliseconds: 500));
    for (final run in await listLocalRuns()) {
      if (!before.contains(run.harnessId) &&
          run.workspace == options.workspaceDir) {
        return run;
      }
    }
    if (exited) {
      throw HarnessStartError(
        'The harness exited before it came up.',
        stderrTail: stderrLines.join('\n'),
      );
    }
  }
  return null;
}
