import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/local_harness_io.dart';
import 'package:silo_app/src/connection/local_harness_options.dart';

void main() {
  test('startLocalHarness reports a missing binary with build guidance',
      () async {
    expect(
      () => startLocalHarness(const LocalHarnessOptions(
        workspaceDir: '/tmp/ws',
        siloBinary: '/nonexistent/path/to/silo',
      )),
      throwsA(isA<HarnessStartError>()
          .having((e) => e.message, 'message',
              contains('/nonexistent/path/to/silo'))
          .having((e) => e.message, 'message',
              contains('cargo build --release'))),
    );
  });
}
