// ChatScreen behavior against a mock channel: the busy indicator and stop
// button, the input-row layout, the connection-details sheet, and the
// regression test for notifying listeners synchronously while an ancestor
// is mid-build.

import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/endpoint.dart';
import 'package:silo_app/src/connection/harness_connection.dart';
import 'package:silo_app/src/connection/message_channel.dart';
import 'package:silo_app/src/connection/secret_store.dart';
import 'package:silo_app/src/ui/chat_screen.dart';
import 'package:silo_app/src/ui/theme.dart';

class MockChannel implements MessageChannel {
  final StreamController<dynamic> _toClient = StreamController<dynamic>();
  final List<Map<String, dynamic>> sent = [];

  @override
  Stream<dynamic> get stream => _toClient.stream;

  @override
  void add(String data) {
    sent.add(jsonDecode(data) as Map<String, dynamic>);
  }

  @override
  Future<void> close() async {
    if (!_toClient.isClosed) {
      await _toClient.close();
    }
  }

  void serverSend(Map<String, dynamic> json) {
    _toClient.add(jsonEncode(json));
  }
}

const endpoint = HarnessEndpoint(
  id: 'ep1',
  name: 'test',
  url: 'wss://127.0.0.1:7777',
);

void main() {
  testWidgets(
      'inserting ChatScreen while an ancestor listens to the connection '
      'does not throw (no notify during build)', (tester) async {
    // The factory never completes, so the connection stays in its first
    // synchronous status change (connecting).
    final connection = HarnessConnection(
      endpoint: endpoint,
      secrets: MemorySecretStore(),
      settings: MemorySecretStore(),
      channelFactory: (uri, fingerprint) => Completer<MessageChannel>().future,
    );

    // Mimics the home screen: a widget listening to the connection is
    // mid-build when ChatScreen (whose initState used to connect, and
    // with it notify, synchronously) is built below it.
    await tester.pumpWidget(MaterialApp(
      home: ListenableBuilder(
        listenable: connection,
        builder: (context, _) => ChatScreen(connection: connection),
      ),
    ));
    expect(tester.takeException(), isNull);

    // The deferred connect has run by the end of the first frame.
    await tester.pump();
    expect(tester.takeException(), isNull);
    expect(connection.status, ConnectionStatus.connecting);

    await tester.pumpWidget(const SizedBox());
    connection.dispose();
  });

  testWidgets(
      'input row orders the text field, then attach, then send, with the '
      'field on the content gutter', (tester) async {
    final connection = HarnessConnection(
      endpoint: endpoint,
      secrets: MemorySecretStore(),
      settings: MemorySecretStore(),
      channelFactory: (uri, fingerprint) => Completer<MessageChannel>().future,
    );

    await tester.pumpWidget(
        MaterialApp(home: ChatScreen(connection: connection)));
    await tester.pump();

    final fieldLeft = tester.getTopLeft(find.byType(TextField)).dx;
    final attachLeft = tester.getTopLeft(find.byTooltip('Upload file')).dx;
    final sendLeft = tester.getTopLeft(find.byTooltip('Send')).dx;
    expect(fieldLeft, contentGutter);
    expect(fieldLeft, lessThan(attachLeft));
    expect(attachLeft, lessThan(sendLeft));

    await tester.pumpWidget(const SizedBox());
    connection.dispose();
  });

  testWidgets('busy shows the stop control and tapping it sends interrupt',
      (tester) async {
    final secrets = MemorySecretStore();
    secrets.values[endpoint.tokenKey] = 'tok';
    final channels = <MockChannel>[];
    final connection = HarnessConnection(
      endpoint: endpoint,
      secrets: secrets,
      settings: MemorySecretStore(),
      channelFactory: (uri, fingerprint) async {
        final channel = MockChannel();
        channels.add(channel);
        return channel;
      },
      backoff: (_) => Duration.zero,
    );

    await tester.pumpWidget(
        MaterialApp(home: ChatScreen(connection: connection)));
    await tester.pump();
    expect(channels, hasLength(1));
    final channel = channels.single;

    // Two pumps per server message: one flushes the stream delivery
    // microtasks, the next builds the frame that reflects them.
    channel.serverSend(
        {'type': 'hello', 'harness_id': 'h-1', 'protocol_version': 1});
    await tester.pump();
    await tester.pump();
    channel.serverSend(
        {'type': 'auth_ok', 'client_id': 'c-1', 'next_seq': 0});
    await tester.pump();
    await tester.pump();
    expect(connection.status, ConnectionStatus.connected);

    // Idle: no stop button, no progress bar, no working status.
    expect(find.byTooltip('Stop'), findsNothing);
    expect(find.byType(LinearProgressIndicator), findsNothing);
    expect(find.text('working…'), findsNothing);

    // A prompt event marks the harness busy.
    channel.serverSend({
      'type': 'event',
      'event': {
        'seq': 0,
        'time': {'logical': 0},
        'kind': 'user_prompt',
        'text': 'go',
      },
    });
    await tester.pump();
    await tester.pump();
    expect(find.byTooltip('Stop'), findsOneWidget);
    expect(find.byType(LinearProgressIndicator), findsOneWidget);
    expect(find.text('working…'), findsOneWidget);

    await tester.tap(find.byTooltip('Stop'));
    await tester.pump();
    expect(channel.sent.last, equals({'type': 'interrupt'}));

    // The interrupted event clears the busy state and renders its tile.
    channel.serverSend({
      'type': 'event',
      'event': {
        'seq': 1,
        'time': {'logical': 1},
        'kind': 'interrupted',
        'agent': 'agent-0',
      },
    });
    await tester.pump();
    await tester.pump();
    expect(find.text('interrupted by the user'), findsOneWidget);
    expect(find.byTooltip('Stop'), findsNothing);
    expect(find.byType(LinearProgressIndicator), findsNothing);

    await tester.pumpWidget(const SizedBox());
    connection.dispose();
  });

  testWidgets(
      'connection details sheet lists the raw identifiers and its switch '
      'reveals suppressed tool tiles', (tester) async {
    final secrets = MemorySecretStore();
    secrets.values[endpoint.tokenKey] = 'tok';
    final channels = <MockChannel>[];
    final connection = HarnessConnection(
      endpoint: endpoint,
      secrets: secrets,
      settings: MemorySecretStore(),
      channelFactory: (uri, fingerprint) async {
        final channel = MockChannel();
        channels.add(channel);
        return channel;
      },
      backoff: (_) => Duration.zero,
    );

    await tester.pumpWidget(
        MaterialApp(home: ChatScreen(connection: connection)));
    await tester.pump();
    final channel = channels.single;

    channel.serverSend(
        {'type': 'hello', 'harness_id': 'h-1', 'protocol_version': 3});
    await tester.pump();
    await tester.pump();
    channel.serverSend(
        {'type': 'auth_ok', 'client_id': 'c-1', 'next_seq': 0});
    await tester.pump();
    await tester.pump();

    // An AskUserQuestion tool call arrives; its raw tile is suppressed.
    channel.serverSend({
      'type': 'event',
      'event': {
        'seq': 0,
        'time': {'logical': 0},
        'kind': 'tool_use',
        'agent': 'agent-0',
        'call': {
          'id': 't-q',
          'name': 'AskUserQuestion',
          'input': {'question': 'Which color?'},
        },
      },
    });
    await tester.pump();
    await tester.pump();
    expect(find.textContaining('AskUserQuestion'), findsNothing);
    // The raw harness id stays out of the app bar.
    expect(find.text('test'), findsOneWidget);
    expect(find.textContaining('h-1'), findsNothing);

    // Clear the busy state so its animated progress bar does not keep
    // pumpAndSettle from settling.
    channel.serverSend({
      'type': 'event',
      'event': {
        'seq': 1,
        'time': {'logical': 1},
        'kind': 'interrupted',
        'agent': 'agent-0',
      },
    });
    await tester.pump();
    await tester.pump();

    await tester.tap(find.byType(PopupMenuButton<String>));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Connection details'));
    await tester.pumpAndSettle();

    expect(find.text('h-1'), findsOneWidget);
    expect(find.text('c-1'), findsOneWidget);
    expect(find.text('3'), findsOneWidget);
    expect(find.text('wss://127.0.0.1:7777'), findsOneWidget);
    expect(find.text('Show raw payloads'), findsOneWidget);

    await tester.tap(find.text('Show raw payloads'));
    await tester.pumpAndSettle();
    // Dismiss the sheet via its barrier.
    await tester.tapAt(const Offset(400, 20));
    await tester.pumpAndSettle();

    expect(find.text('AskUserQuestion · t-q'), findsOneWidget);

    await tester.pumpWidget(const SizedBox());
    connection.dispose();
  });
}
