/// Local harness discovery stubs for the web, where there is no local
/// filesystem and no process spawning.
library;

import '../protocol/protocol.dart';
import 'local_harness_options.dart';

bool get localRunsSupported => false;

bool get canSpawnHarness => false;

String? resolveSiloBinary(String? configuredPath) => null;

bool siloBinaryExists(String path) => false;

String stateDir() => '';

Future<List<RunInfo>> listLocalRuns() async => [];

Future<String> readLocalToken(RunInfo info) async =>
    throw UnsupportedError('local harnesses are not available on the web');

Future<bool> isWorkspaceLocked(String dir) async => false;

Future<RunInfo?> startLocalHarness(
  LocalHarnessOptions options, {
  Duration timeout = const Duration(seconds: 30),
}) async =>
    throw UnsupportedError('local harnesses are not available on the web');
