// Display-name preference order: user-given endpoint name, then workspace
// folder name, then the host of the URL — and never the raw harness id.

import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/endpoint.dart';
import 'package:silo_app/src/connection/harness_connection.dart';
import 'package:silo_app/src/connection/secret_store.dart';
import 'package:silo_app/src/protocol/event.dart';
import 'package:silo_app/src/protocol/types.dart';

void main() {
  group('workspaceFolderName', () {
    test('takes the last non-empty path component', () {
      expect(workspaceFolderName('/home/u/myproj'), 'myproj');
      expect(workspaceFolderName('/home/u/myproj/'), 'myproj');
      expect(workspaceFolderName(r'C:\work\proj'), 'proj');
    });

    test('is null without a usable component', () {
      expect(workspaceFolderName(null), isNull);
      expect(workspaceFolderName(''), isNull);
      expect(workspaceFolderName('///'), isNull);
    });
  });

  group('harnessDisplayName', () {
    test('prefers the user-given endpoint name', () {
      expect(
        harnessDisplayName(
          endpointName: 'build box',
          workspace: '/home/u/myproj',
          url: 'wss://10.0.0.5:7777',
        ),
        'build box',
      );
    });

    test('falls back to the workspace folder name', () {
      expect(
        harnessDisplayName(
          endpointName: '  ',
          workspace: '/home/u/myproj',
          url: 'wss://10.0.0.5:7777',
        ),
        'myproj',
      );
    });

    test('falls back to the URL host, then the URL', () {
      expect(
        harnessDisplayName(url: 'wss://10.0.0.5:7777'),
        '10.0.0.5',
      );
      expect(harnessDisplayName(url: 'not a url'), 'not a url');
    });

    test('a name equal to the harness id counts as absent', () {
      expect(
        harnessDisplayName(
          endpointName: 'a1b2c3d4',
          harnessId: 'a1b2c3d4',
          workspace: '/home/u/myproj',
          url: 'wss://10.0.0.5:7777',
        ),
        'myproj',
      );
      expect(
        harnessDisplayName(
          endpointName: 'a1b2c3d4',
          harnessId: 'a1b2c3d4',
          url: 'wss://10.0.0.5:7777',
        ),
        '10.0.0.5',
      );
    });
  });

  test(
      'connection displayName uses the workspace from harness_started when '
      'the endpoint name is the harness id', () {
    final connection = HarnessConnection(
      endpoint: const HarnessEndpoint(
        id: 'e1',
        name: 'a1b2c3d4',
        url: 'wss://10.0.0.5:7777',
        harnessId: 'a1b2c3d4',
      ),
      secrets: MemorySecretStore(),
      settings: MemorySecretStore(),
    );
    expect(connection.displayName, '10.0.0.5');

    connection.store.insert(Event(
      seq: 0,
      time: const Timestamp(logical: 0),
      payload: const HarnessStartedPayload(
        harnessId: 'a1b2c3d4',
        workspace: '/home/u/myproj',
        sandbox: 'mock',
        llm: 'anthropic',
      ),
    ));
    expect(connection.displayName, 'myproj');
    connection.dispose();
  });
}
