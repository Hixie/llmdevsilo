// Drives HarnessConnection's handshake and auth state machine through a
// mock channel: local token, pairing, challenge signature, auth failure,
// and reconnect-with-resume.

import 'dart:async';
import 'dart:convert';

import 'package:cryptography/cryptography.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/endpoint.dart';
import 'package:silo_app/src/connection/harness_connection.dart';
import 'package:silo_app/src/connection/message_channel.dart';
import 'package:silo_app/src/connection/secret_store.dart';

class MockChannel implements MessageChannel {
  final StreamController<dynamic> _toClient = StreamController<dynamic>();
  final List<Map<String, dynamic>> sent = [];
  bool closed = false;

  @override
  Stream<dynamic> get stream => _toClient.stream;

  @override
  void add(String data) {
    sent.add(jsonDecode(data) as Map<String, dynamic>);
  }

  @override
  Future<void> close() async {
    closed = true;
    if (!_toClient.isClosed) {
      await _toClient.close();
    }
  }

  void serverSend(Map<String, dynamic> json) {
    _toClient.add(jsonEncode(json));
  }

  /// Simulates the server dropping the connection.
  Future<void> dropFromServer() async {
    if (!_toClient.isClosed) {
      await _toClient.close();
    }
  }

  /// Waits until at least [count] client messages have been sent.
  Future<void> waitForSent(int count) async {
    for (var i = 0; i < 200; i++) {
      if (sent.length >= count) {
        return;
      }
      await Future<void>.delayed(const Duration(milliseconds: 5));
    }
    fail('expected $count sent messages, got ${sent.length}: $sent');
  }
}

class MockFactory {
  final List<MockChannel> channels = [];
  final List<String?> fingerprints = [];

  Future<MessageChannel> connect(Uri uri, String? fingerprint) async {
    final channel = MockChannel();
    channels.add(channel);
    fingerprints.add(fingerprint);
    return channel;
  }
}

const endpoint = HarnessEndpoint(
  id: 'ep1',
  name: 'test',
  url: 'wss://127.0.0.1:7777',
);

Future<void> waitFor(
  bool Function() condition, {
  String reason = 'condition',
}) async {
  for (var i = 0; i < 200; i++) {
    if (condition()) {
      return;
    }
    await Future<void>.delayed(const Duration(milliseconds: 5));
  }
  fail('timed out waiting for $reason');
}

void main() {
  test('local token auth then backlog request from seq 0', () async {
    final secrets = MemorySecretStore();
    secrets.values[endpoint.tokenKey] = 'a' * 64;
    final factory = MockFactory();
    final connection = HarnessConnection(
      endpoint: endpoint,
      secrets: secrets,
      channelFactory: factory.connect,
      backoff: (_) => Duration.zero,
    );

    await connection.connect();
    final channel = factory.channels.single;
    expect(connection.status, ConnectionStatus.connecting);

    channel.serverSend(
        {'type': 'hello', 'harness_id': 'h-1', 'protocol_version': 1});
    await channel.waitForSent(1);
    expect(connection.status, ConnectionStatus.authenticating);
    expect(channel.sent[0], {
      'type': 'authenticate',
      'method': 'local_token',
      'token': 'a' * 64,
    });

    channel.serverSend(
        {'type': 'auth_ok', 'client_id': 'c-1', 'next_seq': 0});
    await channel.waitForSent(4);
    expect(connection.status, ConnectionStatus.connected);
    expect(channel.sent[1], {'type': 'request_events', 'from_seq': 0});
    expect(channel.sent[2], {'type': 'request_access_report'});
    expect(channel.sent[3], {'type': 'request_cost'});
    expect(connection.harnessId, 'h-1');
    expect(connection.clientId, 'c-1');

    await connection.disconnect();
  });

  test('pairing generates a key pair and stores the seed and key id',
      () async {
    final secrets = MemorySecretStore();
    final factory = MockFactory();
    final connection = HarnessConnection(
      endpoint: endpoint,
      secrets: secrets,
      channelFactory: factory.connect,
      clientName: 'phone',
      backoff: (_) => Duration.zero,
    );
    connection.pendingPairingCode = 'A1B2C3D4';

    await connection.connect();
    final channel = factory.channels.single;
    channel.serverSend(
        {'type': 'hello', 'harness_id': 'h-1', 'protocol_version': 1});
    await channel.waitForSent(1);

    final pair = channel.sent[0];
    expect(pair['type'], 'authenticate');
    expect(pair['method'], 'pair');
    expect(pair['code'], 'A1B2C3D4');
    expect(pair['client_name'], 'phone');
    final publicKey = base64Decode(pair['public_key_b64'] as String);
    expect(publicKey.length, 32);

    channel.serverSend({
      'type': 'auth_ok',
      'client_id': 'c-2',
      'key_id': 'key-9',
      'next_seq': 0,
    });
    await channel.waitForSent(4);
    expect(connection.status, ConnectionStatus.connected);
    expect(connection.pendingPairingCode, isNull);
    expect(secrets.values[endpoint.keyIdKey], 'key-9');

    // The stored seed reproduces the public key sent during pairing.
    final seed = base64Decode(secrets.values[endpoint.keySeedKey]!);
    final algorithm = Ed25519();
    final keyPair = await algorithm.newKeyPairFromSeed(seed);
    final derived = await keyPair.extractPublicKey();
    expect(derived.bytes, publicKey);

    await connection.disconnect();
  });

  test('returning client signs the challenge with its stored key', () async {
    final algorithm = Ed25519();
    final keyPair = await algorithm.newKeyPair();
    final seed = await keyPair.extractPrivateKeyBytes();
    final publicKey = await keyPair.extractPublicKey();

    final secrets = MemorySecretStore();
    secrets.values[endpoint.keyIdKey] = 'key-1';
    secrets.values[endpoint.keySeedKey] = base64Encode(seed);
    final factory = MockFactory();
    final connection = HarnessConnection(
      endpoint: endpoint,
      secrets: secrets,
      channelFactory: factory.connect,
      backoff: (_) => Duration.zero,
    );

    await connection.connect();
    final channel = factory.channels.single;
    channel.serverSend(
        {'type': 'hello', 'harness_id': 'h-1', 'protocol_version': 1});
    await channel.waitForSent(1);
    expect(channel.sent[0], {
      'type': 'authenticate',
      'method': 'challenge',
      'key_id': 'key-1',
    });

    final challenge = utf8.encode('challenge-bytes-123');
    channel.serverSend({
      'type': 'auth_challenge',
      'challenge_b64': base64Encode(challenge),
    });
    await channel.waitForSent(2);
    final response = channel.sent[1];
    expect(response['type'], 'authenticate');
    expect(response['method'], 'signature');
    expect(response['key_id'], 'key-1');

    // The signature must verify against the registered public key.
    final signature = Signature(
      base64Decode(response['signature_b64'] as String),
      publicKey: publicKey,
    );
    expect(
      await algorithm.verify(challenge, signature: signature),
      isTrue,
    );

    channel.serverSend(
        {'type': 'auth_ok', 'client_id': 'c-3', 'next_seq': 5});
    await channel.waitForSent(5);
    expect(connection.status, ConnectionStatus.connected);

    await connection.disconnect();
  });

  test('auth_error fails the connection without reconnecting', () async {
    final secrets = MemorySecretStore();
    secrets.values[endpoint.tokenKey] = 'bad';
    final factory = MockFactory();
    final connection = HarnessConnection(
      endpoint: endpoint,
      secrets: secrets,
      channelFactory: factory.connect,
      backoff: (_) => Duration.zero,
    );

    await connection.connect();
    final channel = factory.channels.single;
    channel.serverSend(
        {'type': 'hello', 'harness_id': 'h-1', 'protocol_version': 1});
    await channel.waitForSent(1);
    channel.serverSend({'type': 'auth_error', 'message': 'bad token'});
    await waitFor(() => connection.status == ConnectionStatus.failed,
        reason: 'failed status');
    expect(connection.lastError, 'bad token');

    // No second channel is opened.
    await Future<void>.delayed(const Duration(milliseconds: 50));
    expect(factory.channels.length, 1);
  });

  test('no credentials at all fails cleanly', () async {
    final secrets = MemorySecretStore();
    final factory = MockFactory();
    final connection = HarnessConnection(
      endpoint: endpoint,
      secrets: secrets,
      channelFactory: factory.connect,
      backoff: (_) => Duration.zero,
    );

    await connection.connect();
    factory.channels.single.serverSend(
        {'type': 'hello', 'harness_id': 'h-1', 'protocol_version': 1});
    await waitFor(() => connection.status == ConnectionStatus.failed,
        reason: 'failed status');
    expect(connection.lastError, contains('pair again'));
  });

  test('reconnect resumes the event stream from the next sequence number',
      () async {
    final secrets = MemorySecretStore();
    secrets.values[endpoint.tokenKey] = 'tok';
    final factory = MockFactory();
    final connection = HarnessConnection(
      endpoint: endpoint,
      secrets: secrets,
      channelFactory: factory.connect,
      backoff: (_) => Duration.zero,
    );

    await connection.connect();
    var channel = factory.channels.single;
    channel.serverSend(
        {'type': 'hello', 'harness_id': 'h-1', 'protocol_version': 1});
    await channel.waitForSent(1);
    channel.serverSend(
        {'type': 'auth_ok', 'client_id': 'c-1', 'next_seq': 0});
    await channel.waitForSent(4);

    // Backlog plus one live event.
    channel.serverSend({
      'type': 'events',
      'events': [
        {
          'seq': 0,
          'time': {'logical': 0},
          'kind': 'user_prompt',
          'text': 'hi',
        },
        {
          'seq': 1,
          'time': {'logical': 1},
          'kind': 'assistant_text',
          'agent': 'agent-0',
          'text': 'hello',
        },
      ],
    });
    channel.serverSend({
      'type': 'event',
      'event': {
        'seq': 2,
        'time': {'logical': 2},
        'kind': 'awaiting_input',
      },
    });
    await waitFor(() => connection.store.length == 3, reason: '3 events');

    // Server drops the connection; the client reconnects and resumes.
    await channel.dropFromServer();
    await waitFor(() => factory.channels.length == 2,
        reason: 'second channel');
    channel = factory.channels[1];
    channel.serverSend(
        {'type': 'hello', 'harness_id': 'h-1', 'protocol_version': 1});
    await channel.waitForSent(1);
    channel.serverSend(
        {'type': 'auth_ok', 'client_id': 'c-1', 'next_seq': 4});
    await channel.waitForSent(2);
    expect(channel.sent[1], {'type': 'request_events', 'from_seq': 3});

    // Overlapping backlog is de-duplicated by seq.
    channel.serverSend({
      'type': 'events',
      'events': [
        {
          'seq': 2,
          'time': {'logical': 2},
          'kind': 'awaiting_input',
        },
        {
          'seq': 3,
          'time': {'logical': 3},
          'kind': 'user_prompt',
          'text': 'again',
        },
      ],
    });
    await waitFor(() => connection.store.length == 4, reason: '4 events');
    expect(connection.store.events.map((e) => e.seq).toList(), [0, 1, 2, 3]);

    await connection.disconnect();
  });

  test('pinned fingerprint is passed to the channel factory', () async {
    final secrets = MemorySecretStore();
    secrets.values[endpoint.tokenKey] = 'tok';
    secrets.values[endpoint.fingerprintKey] = 'deadbeef';
    final factory = MockFactory();
    final connection = HarnessConnection(
      endpoint: endpoint,
      secrets: secrets,
      channelFactory: factory.connect,
      backoff: (_) => Duration.zero,
    );
    await connection.connect();
    expect(factory.fingerprints.single, 'deadbeef');
    await connection.disconnect();
  });
}
