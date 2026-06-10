// Run-file listing: pid liveness filtering with an injected probe, and
// the default kill-0 probe.

import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/local_harness_io.dart';

Map<String, dynamic> runFixture(String harnessId, int pid) => {
      'harness_id': harnessId,
      'addr': '127.0.0.1:7777',
      'cert_fingerprint_sha256': 'ab' * 32,
      'local_token_path': '/tmp/$harnessId/local-token',
      'pid': pid,
      'workspace': '/work/$harnessId',
    };

void main() {
  test('listLocalRuns hides runs whose pid is dead, without deleting them',
      () async {
    final dir = await Directory.systemTemp.createTemp('silo-runs-');
    addTearDown(() => dir.delete(recursive: true));
    File('${dir.path}/h-live.json')
        .writeAsStringSync(jsonEncode(runFixture('h-live', 42)));
    File('${dir.path}/h-dead.json')
        .writeAsStringSync(jsonEncode(runFixture('h-dead', 43)));
    File('${dir.path}/notes.txt').writeAsStringSync('not a run file');

    final probed = <int>[];
    final runs = await listLocalRuns(
      runDir: dir.path,
      isAlive: (pid) async {
        probed.add(pid);
        return pid == 42;
      },
    );

    expect(runs.map((r) => r.harnessId).toList(), ['h-live']);
    expect(probed.toSet(), {42, 43});
    // Stale run files stay on disk; the silo CLI owns pruning.
    expect(File('${dir.path}/h-dead.json').existsSync(), isTrue);
  });

  test('listLocalRuns of a missing directory is empty', () async {
    final runs = await listLocalRuns(
      runDir: '/nonexistent/silo-test-run-dir',
      isAlive: (_) async => true,
    );
    expect(runs, isEmpty);
  });

  test('pidIsAlive sees the current process as alive', () async {
    expect(await pidIsAlive(pid), isTrue);
  });

  test('pidIsAlive sees an impossible pid as dead', () async {
    if (!Platform.isMacOS && !Platform.isLinux) {
      return;
    }
    // Far beyond any real pid range.
    expect(await pidIsAlive(999999999), isFalse);
  });

  test('pidIsAlive treats a permission error as alive', () async {
    if (!Platform.isMacOS && !Platform.isLinux) {
      return;
    }
    // kill -0 fails with EPERM for a live process owned by another user.
    expect(
      await pidIsAlive(
        4242,
        runProcess: (executable, arguments) async =>
            ProcessResult(0, 1, '', 'kill: 4242: Operation not permitted'),
      ),
      isTrue,
    );
    // Any other failure still counts as dead.
    expect(
      await pidIsAlive(
        4242,
        runProcess: (executable, arguments) async =>
            ProcessResult(0, 1, '', 'kill: 4242: No such process'),
      ),
      isFalse,
    );
  });
}
