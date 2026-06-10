import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/silo_locator.dart';

void main() {
  /// A resolver call with [existing] as the set of paths that exist.
  String? locate({
    String? configuredPath,
    Map<String, String> environment = const {},
    Set<String> existing = const {},
  }) =>
      locateSilo(
        configuredPath: configuredPath,
        environment: environment,
        fileExists: existing.contains,
      );

  group('locateSilo', () {
    test('the configured path wins over every other candidate', () {
      expect(
        locate(
          configuredPath: '/custom/silo',
          environment: {
            'SILO_BIN': '/env/silo',
            'PATH': '/bin',
            'HOME': '/home/u',
          },
          existing: {
            '/custom/silo',
            '/env/silo',
            '/bin/silo',
            '/home/u/.cargo/bin/silo',
          },
        ),
        '/custom/silo',
      );
    });

    test('a non-existent configured path is skipped for later candidates',
        () {
      expect(
        locate(
          configuredPath: '/gone/silo',
          environment: {'SILO_BIN': '/env/silo'},
          existing: {'/env/silo'},
        ),
        '/env/silo',
      );
    });

    test('SILO_BIN is used when no configured path is set', () {
      expect(
        locate(
          environment: {'SILO_BIN': '/env/silo', 'PATH': '/bin'},
          existing: {'/env/silo', '/bin/silo'},
        ),
        '/env/silo',
      );
    });

    test('PATH entries are probed in order', () {
      expect(
        locate(
          environment: {'PATH': '/first:/second:/third'},
          existing: {'/second/silo', '/third/silo'},
        ),
        '/second/silo',
      );
    });

    test('empty PATH entries are skipped', () {
      expect(
        locate(
          environment: {'PATH': ':/bin::'},
          existing: {'/bin/silo'},
        ),
        '/bin/silo',
      );
      // An empty entry never produces the bare candidate "/silo".
      expect(
        locate(environment: {'PATH': '::'}, existing: {'/silo'}),
        isNull,
      );
    });

    test('falls back to cargo bin, then homebrew, then /usr/local', () {
      const env = {'HOME': '/home/u', 'PATH': '/bin'};
      expect(
        locate(
          environment: env,
          existing: {
            '/home/u/.cargo/bin/silo',
            '/opt/homebrew/bin/silo',
            '/usr/local/bin/silo',
          },
        ),
        '/home/u/.cargo/bin/silo',
      );
      expect(
        locate(
          environment: env,
          existing: {'/opt/homebrew/bin/silo', '/usr/local/bin/silo'},
        ),
        '/opt/homebrew/bin/silo',
      );
      expect(
        locate(environment: env, existing: {'/usr/local/bin/silo'}),
        '/usr/local/bin/silo',
      );
    });

    test('returns null when no candidate exists', () {
      expect(
        locate(
          configuredPath: '/gone/silo',
          environment: {
            'SILO_BIN': '/env/silo',
            'PATH': '/a:/b',
            'HOME': '/home/u',
          },
        ),
        isNull,
      );
      expect(locate(), isNull);
    });

    test('without HOME the cargo bin candidate is omitted', () {
      expect(
        siloCandidates(environment: const {}),
        ['/opt/homebrew/bin/silo', '/usr/local/bin/silo'],
      );
    });
  });
}
