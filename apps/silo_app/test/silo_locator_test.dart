import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/silo_locator.dart';

void main() {
  /// A resolver call with [existing] as the set of paths that exist.
  String? locate({
    String? configuredPath,
    Map<String, String> environment = const {},
    Set<String> existing = const {},
    String? executablePath,
  }) =>
      locateSilo(
        configuredPath: configuredPath,
        environment: environment,
        fileExists: existing.contains,
        executablePath: executablePath,
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

    test('workspace target builds above the executable are found', () {
      // The development app bundle lives inside the repository, so walking
      // up from the executable reaches the repository root and its target/
      // directory.
      const exe = '/repo/apps/silo_app/build/macos/Build/Products/Debug/'
          'Silo.app/Contents/MacOS/Silo';
      expect(
        locate(
          executablePath: exe,
          existing: {'/repo/target/debug/silo'},
        ),
        '/repo/target/debug/silo',
      );
      // A release build is preferred over a debug build of the same root.
      expect(
        locate(
          executablePath: exe,
          existing: {'/repo/target/debug/silo', '/repo/target/release/silo'},
        ),
        '/repo/target/release/silo',
      );
      // Nearer ancestors win over the repository root.
      expect(
        locate(
          executablePath: exe,
          existing: {
            '/repo/apps/silo_app/target/release/silo',
            '/repo/target/release/silo',
          },
        ),
        '/repo/apps/silo_app/target/release/silo',
      );
    });

    test('the app bundle Helpers directory is probed', () {
      // A release bundle embeds the CLI binaries at Contents/Helpers/.
      const exe = '/Applications/Silo.app/Contents/MacOS/Silo';
      expect(
        locate(
          executablePath: exe,
          existing: {'/Applications/Silo.app/Contents/Helpers/silo'},
        ),
        '/Applications/Silo.app/Contents/Helpers/silo',
      );
    });

    test('PATH wins over the bundle Helpers candidate', () {
      expect(
        locate(
          executablePath: '/Applications/Silo.app/Contents/MacOS/Silo',
          environment: {'PATH': '/bin'},
          existing: {
            '/bin/silo',
            '/Applications/Silo.app/Contents/Helpers/silo',
          },
        ),
        '/bin/silo',
      );
    });

    test('the bundle Helpers candidate wins over workspace builds', () {
      const exe = '/repo/apps/silo_app/build/macos/Build/Products/Release/'
          'Silo.app/Contents/MacOS/Silo';
      expect(
        locate(
          executablePath: exe,
          existing: {
            '/repo/apps/silo_app/build/macos/Build/Products/Release/'
                'Silo.app/Contents/Helpers/silo',
            '/repo/target/release/silo',
          },
        ),
        '/repo/apps/silo_app/build/macos/Build/Products/Release/'
        'Silo.app/Contents/Helpers/silo',
      );
    });

    test('a path with fewer than two ancestors adds no Helpers candidate',
        () {
      expect(
        siloCandidates(environment: const {}, executablePath: '/Silo'),
        ['/opt/homebrew/bin/silo', '/usr/local/bin/silo'],
      );
    });

    test('PATH and SILO_BIN take precedence over workspace builds', () {
      expect(
        locate(
          executablePath: '/repo/apps/app.app/Contents/MacOS/app',
          environment: {'PATH': '/bin'},
          existing: {'/bin/silo', '/repo/target/release/silo'},
        ),
        '/bin/silo',
      );
    });

    test('relative or absent executable paths add no candidates', () {
      expect(
        siloCandidates(environment: const {}, executablePath: 'Silo'),
        ['/opt/homebrew/bin/silo', '/usr/local/bin/silo'],
      );
      expect(
        siloCandidates(environment: const {}, executablePath: null),
        ['/opt/homebrew/bin/silo', '/usr/local/bin/silo'],
      );
    });
  });
}
