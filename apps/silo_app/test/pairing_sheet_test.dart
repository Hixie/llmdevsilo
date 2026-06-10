// Widget tests for the pairing sheet: section presence, the LAN-candidate
// section and loopback warning, the copy-all clipboard block, and the
// expiry countdown.

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/ui/pairing_sheet.dart';

const String _fingerprint =
    'c2a7f31e88d04b6a915c2e7d3fa6b09812ce45d7a3f0b16c84d29e5b7a01c6f3';

Widget _app(PairingSheet sheet) =>
    MaterialApp(home: Scaffold(body: sheet));

Future<void> _pumpSheet(
  WidgetTester tester, {
  required String url,
  String? fingerprint = _fingerprint,
  String? code = 'AB12CD34',
  int? expiresInSecs = 120,
  Future<List<String>> Function()? lanAddresses,
}) async {
  tester.view.physicalSize = const Size(800, 1100);
  tester.view.devicePixelRatio = 1.0;
  addTearDown(tester.view.reset);
  await tester.pumpWidget(_app(PairingSheet(
    url: url,
    fingerprint: fingerprint,
    code: code,
    expiresInSecs: expiresInSecs,
    lanAddresses: lanAddresses ?? () async => const [],
  )));
  // Resolve the LAN address future.
  await tester.pump();
}

void main() {
  testWidgets('shows code, countdown, URL, fingerprint, and browser hint',
      (tester) async {
    await _pumpSheet(tester, url: 'wss://192.168.1.9:7777');

    expect(find.text('AB12CD34'), findsOneWidget);
    expect(find.text('Expires in 120 s'), findsOneWidget);
    expect(find.text('wss://192.168.1.9:7777'), findsOneWidget);
    expect(find.text(_fingerprint), findsOneWidget);
    expect(find.text('Certificate fingerprint (SHA-256)'), findsOneWidget);
    expect(find.textContaining('https://192.168.1.9:7777/'), findsOneWidget);
    expect(find.text('Copy connection details'), findsOneWidget);
  });

  testWidgets('non-loopback host shows no LAN section and no warning',
      (tester) async {
    await _pumpSheet(
      tester,
      url: 'wss://192.168.1.9:7777',
      lanAddresses: () async => ['10.0.0.7'],
    );

    expect(find.text('Addresses on your network'), findsNothing);
    expect(find.text('wss://10.0.0.7:7777'), findsNothing);
    expect(find.textContaining('--listen'), findsNothing);
  });

  testWidgets('loopback host lists LAN candidates and the --listen warning',
      (tester) async {
    await _pumpSheet(
      tester,
      url: 'wss://127.0.0.1:7777',
      lanAddresses: () async => ['192.168.1.5', '10.0.0.3'],
    );

    expect(find.text('Addresses on your network'), findsOneWidget);
    expect(find.text('wss://192.168.1.5:7777'), findsOneWidget);
    expect(find.text('wss://10.0.0.3:7777'), findsOneWidget);
    expect(find.textContaining('--listen 0.0.0.0:7777'), findsOneWidget);
  });

  testWidgets('unspecified host lists LAN candidates without the warning',
      (tester) async {
    await _pumpSheet(
      tester,
      url: 'wss://0.0.0.0:7777',
      lanAddresses: () async => ['192.168.1.5'],
    );

    expect(find.text('Addresses on your network'), findsOneWidget);
    expect(find.text('wss://192.168.1.5:7777'), findsOneWidget);
    expect(find.textContaining('--listen'), findsNothing);
  });

  testWidgets('copy-all puts the assembled details block on the clipboard',
      (tester) async {
    final calls = <MethodCall>[];
    tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
      SystemChannels.platform,
      (call) async {
        calls.add(call);
        return null;
      },
    );
    addTearDown(() => tester.binding.defaultBinaryMessenger
        .setMockMethodCallHandler(SystemChannels.platform, null));

    await _pumpSheet(
      tester,
      url: 'wss://127.0.0.1:7777',
      fingerprint: 'aa11',
      lanAddresses: () async => ['192.168.1.5'],
    );

    await tester.ensureVisible(find.text('Copy connection details'));
    await tester.tap(find.text('Copy connection details'));
    await tester.pump();

    final setData = calls.lastWhere((c) => c.method == 'Clipboard.setData');
    expect(
      (setData.arguments as Map<Object?, Object?>)['text'],
      'WebSocket URLs:\n'
      '  wss://127.0.0.1:7777\n'
      '  wss://192.168.1.5:7777\n'
      'Certificate fingerprint (SHA-256): aa11\n'
      'Pairing code: AB12CD34\n',
    );
    expect(find.text('Connection details copied'), findsOneWidget);
  });

  testWidgets('countdown ticks down and reports expiry', (tester) async {
    await _pumpSheet(
      tester,
      url: 'wss://192.168.1.9:7777',
      expiresInSecs: 3,
    );

    expect(find.text('Expires in 3 s'), findsOneWidget);
    await tester.pump(const Duration(seconds: 1));
    expect(find.text('Expires in 2 s'), findsOneWidget);
    await tester.pump(const Duration(seconds: 1));
    expect(find.text('Expires in 1 s'), findsOneWidget);
    await tester.pump(const Duration(seconds: 1));
    expect(
      find.text('Code expired — close this sheet and request a new one.'),
      findsOneWidget,
    );
    // The timer stops at zero; pumping further changes nothing.
    await tester.pump(const Duration(seconds: 5));
    expect(
      find.text('Code expired — close this sheet and request a new one.'),
      findsOneWidget,
    );
  });

  testWidgets('shows a progress indicator until the code arrives',
      (tester) async {
    await _pumpSheet(
      tester,
      url: 'wss://192.168.1.9:7777',
      code: null,
      expiresInSecs: null,
    );

    expect(find.byType(CircularProgressIndicator), findsOneWidget);
    expect(find.text('Requesting a pairing code…'), findsOneWidget);
  });
}
