/// One live connection to a harness's interactive frontend.
///
/// Handles the Hello handshake, the three authentication methods (local
/// token, pairing code + fresh Ed25519 key pair, challenge signature with a
/// stored key), event backlog catch-up, the live event stream, and
/// reconnection with resume.
library;

import 'dart:async';
import 'dart:convert';

import 'package:cryptography/cryptography.dart';
import 'package:flutter/foundation.dart';

import '../protocol/event.dart';
import '../protocol/protocol.dart';
import '../protocol/sandbox.dart';
import 'default_channel.dart' as default_channel;
import 'endpoint.dart';
import 'event_store.dart';
import 'message_channel.dart';
import 'secret_store.dart';

enum ConnectionStatus {
  /// Not connected and not trying to connect.
  disconnected,

  /// Opening the socket or waiting for Hello.
  connecting,

  /// Socket open; authentication in progress.
  authenticating,

  /// Authenticated; receiving events.
  connected,

  /// Connection lost; a reconnect attempt is scheduled.
  reconnecting,

  /// Authentication was rejected or no credentials are available.
  /// No automatic reconnection.
  failed,
}

/// Computes the delay before reconnect attempt [attempt] (starting at zero).
typedef BackoffPolicy = Duration Function(int attempt);

Duration _defaultBackoff(int attempt) {
  final seconds = 1 << (attempt < 5 ? attempt : 5);
  return Duration(seconds: seconds);
}

class HarnessConnection extends ChangeNotifier {
  HarnessConnection({
    required this.endpoint,
    required this._secrets,
    ChannelFactory? channelFactory,
    this.clientName = 'silo_app',
    BackoffPolicy? backoff,
  })  : _channelFactory = channelFactory ?? default_channel.platformConnect,
        _backoff = backoff ?? _defaultBackoff;

  final HarnessEndpoint endpoint;
  final String clientName;
  final EventStore store = EventStore();

  final SecretStore _secrets;
  final ChannelFactory _channelFactory;
  final BackoffPolicy _backoff;

  MessageChannel? _channel;
  StreamSubscription<dynamic>? _subscription;
  Timer? _reconnectTimer;
  int _reconnectAttempt = 0;
  bool _closing = false;
  bool _disposed = false;

  /// Pairing code to redeem on the next connection attempt. Cleared once
  /// pairing succeeds.
  String? pendingPairingCode;

  /// Ed25519 key pair generated for pairing, held until the server confirms
  /// with AuthOk (which carries the assigned key id).
  List<int>? _pendingKeySeed;

  ConnectionStatus _status = ConnectionStatus.disconnected;
  ConnectionStatus get status => _status;

  String? lastError;
  String? harnessId;
  String? clientId;
  AccessReport? accessReport;
  List<CostEntry> costEntries = const [];

  /// Last pairing code issued by the harness via RequestPairingCode.
  PairingCodeMessage? issuedPairingCode;

  /// Message from the server's ShuttingDown notice, if one arrived.
  String? shutdownMessage;

  int _lastReadSeq = -1;

  /// Events past the last point the user viewed this harness's transcript.
  int get unreadCount =>
      store.events.where((e) => e.seq > _lastReadSeq).length;

  void markRead() {
    final events = store.events;
    if (events.isNotEmpty) {
      _lastReadSeq = events.last.seq;
      notifyListeners();
    }
  }

  void _setStatus(ConnectionStatus status) {
    _status = status;
    if (!_disposed) {
      notifyListeners();
    }
  }

  /// Opens the connection. Safe to call when already connected (no-op).
  Future<void> connect() async {
    if (_status == ConnectionStatus.connecting ||
        _status == ConnectionStatus.authenticating ||
        _status == ConnectionStatus.connected) {
      return;
    }
    _closing = false;
    _reconnectTimer?.cancel();
    _setStatus(ConnectionStatus.connecting);
    try {
      final fingerprint = await _secrets.read(endpoint.fingerprintKey);
      final channel =
          await _channelFactory(Uri.parse(endpoint.url), fingerprint);
      _channel = channel;
      _subscription = channel.stream.listen(
        _onFrame,
        onError: (Object error) => _onDisconnected('$error'),
        onDone: () => _onDisconnected(null),
        cancelOnError: true,
      );
    } catch (error) {
      lastError = '$error';
      _scheduleReconnect();
    }
  }

  /// Closes the connection and stops reconnecting.
  Future<void> disconnect() async {
    _closing = true;
    _reconnectTimer?.cancel();
    await _subscription?.cancel();
    _subscription = null;
    await _channel?.close();
    _channel = null;
    _setStatus(ConnectionStatus.disconnected);
  }

  void _onDisconnected(String? error) {
    if (error != null) {
      lastError = error;
    }
    _subscription = null;
    _channel = null;
    if (_closing || _status == ConnectionStatus.failed) {
      if (_status != ConnectionStatus.failed) {
        _setStatus(ConnectionStatus.disconnected);
      }
      return;
    }
    _scheduleReconnect();
  }

  void _scheduleReconnect() {
    if (_closing || _disposed) {
      _setStatus(ConnectionStatus.disconnected);
      return;
    }
    _setStatus(ConnectionStatus.reconnecting);
    final delay = _backoff(_reconnectAttempt);
    _reconnectAttempt += 1;
    _reconnectTimer?.cancel();
    _reconnectTimer = Timer(delay, () {
      _setStatus(ConnectionStatus.disconnected);
      connect();
    });
  }

  void _send(ClientMessage message) {
    final channel = _channel;
    if (channel == null) {
      return;
    }
    channel.add(jsonEncode(message.toJson()));
  }

  Future<void> _onFrame(dynamic frame) async {
    if (frame is! String) {
      return;
    }
    ServerMessage message;
    try {
      message =
          ServerMessage.fromJson(jsonDecode(frame) as Map<String, dynamic>);
    } catch (error) {
      lastError = 'bad server message: $error';
      notifyListeners();
      return;
    }
    await _handleMessage(message);
  }

  Future<void> _handleMessage(ServerMessage message) async {
    switch (message) {
      case HelloMessage():
        harnessId = message.harnessId;
        _setStatus(ConnectionStatus.authenticating);
        await _startAuth();
      case AuthChallengeMessage():
        await _answerChallenge(message.challengeB64);
      case AuthOkMessage():
        await _onAuthOk(message);
      case AuthErrorMessage():
        lastError = message.message;
        _closing = true;
        _setStatus(ConnectionStatus.failed);
        await _channel?.close();
      case EventMessage():
        store.insert(message.event);
        _applyEventSideEffects(message.event);
        notifyListeners();
      case EventsMessage():
        store.insertAll(message.events);
        for (final event in message.events) {
          _applyEventSideEffects(event);
        }
        notifyListeners();
      case AccessReportMessage():
        accessReport = message.report;
        notifyListeners();
      case CostMessage():
        costEntries = message.entries;
        notifyListeners();
      case PairingCodeMessage():
        issuedPairingCode = message;
        notifyListeners();
      case PongMessage():
        break;
      case ErrorMessage():
        lastError = message.message;
        notifyListeners();
      case ShuttingDownMessage():
        shutdownMessage = message.message ?? 'Harness shut down';
        _closing = true;
        notifyListeners();
    }
  }

  void _applyEventSideEffects(Event event) {
    final payload = event.payload;
    if (payload is AccessReportUpdatedPayload) {
      accessReport = payload.report;
    }
  }

  /// Picks an authentication method. Preference order: local token, stored
  /// pairing key (challenge/signature), pairing code.
  Future<void> _startAuth() async {
    final token = await _secrets.read(endpoint.tokenKey);
    if (token != null) {
      _send(AuthenticateMessage(auth: LocalTokenAuth(token: token)));
      return;
    }
    final keyId = await _secrets.read(endpoint.keyIdKey);
    final seed = await _secrets.read(endpoint.keySeedKey);
    if (keyId != null && seed != null) {
      _send(AuthenticateMessage(auth: ChallengeAuth(keyId: keyId)));
      return;
    }
    final code = pendingPairingCode;
    if (code != null) {
      final algorithm = Ed25519();
      final keyPair = await algorithm.newKeyPair();
      _pendingKeySeed = await keyPair.extractPrivateKeyBytes();
      final publicKey = await keyPair.extractPublicKey();
      _send(AuthenticateMessage(
        auth: PairAuth(
          code: code,
          publicKeyB64: base64Encode(publicKey.bytes),
          clientName: clientName,
        ),
      ));
      return;
    }
    lastError = 'no credentials for this harness; pair again';
    _closing = true;
    _setStatus(ConnectionStatus.failed);
    await _channel?.close();
  }

  Future<void> _answerChallenge(String challengeB64) async {
    final keyId = await _secrets.read(endpoint.keyIdKey);
    final seed = await _secrets.read(endpoint.keySeedKey);
    if (keyId == null || seed == null) {
      lastError = 'challenge received but no stored key';
      _setStatus(ConnectionStatus.failed);
      return;
    }
    final algorithm = Ed25519();
    final keyPair = await algorithm.newKeyPairFromSeed(base64Decode(seed));
    final signature =
        await algorithm.sign(base64Decode(challengeB64), keyPair: keyPair);
    _send(AuthenticateMessage(
      auth: SignatureAuth(
        keyId: keyId,
        signatureB64: base64Encode(signature.bytes),
      ),
    ));
  }

  Future<void> _onAuthOk(AuthOkMessage message) async {
    clientId = message.clientId;
    final seed = _pendingKeySeed;
    if (seed != null && message.keyId != null) {
      await _secrets.write(endpoint.keySeedKey, base64Encode(seed));
      await _secrets.write(endpoint.keyIdKey, message.keyId!);
      _pendingKeySeed = null;
      pendingPairingCode = null;
    }
    _reconnectAttempt = 0;
    _setStatus(ConnectionStatus.connected);
    _send(RequestEventsMessage(fromSeq: store.nextSeq));
    _send(const RequestAccessReportMessage());
    _send(const RequestCostMessage());
  }

  // Send helpers.

  void sendPrompt(String text) => _send(PromptMessage(text: text));

  void answerQuestion(String questionId, String answer) =>
      _send(AnswerQuestionMessage(questionId: questionId, answer: answer));

  void uploadFile(String name, List<int> bytes) => _send(
      UploadFileMessage(name: name, contentB64: base64Encode(bytes)));

  void requestAccessReport() => _send(const RequestAccessReportMessage());

  void requestCost() => _send(const RequestCostMessage());

  void requestPairingCode() {
    issuedPairingCode = null;
    _send(const RequestPairingCodeMessage());
  }

  /// The pinned certificate fingerprint for this endpoint, if one is
  /// stored (hex SHA-256).
  Future<String?> pinnedFingerprint() => _secrets.read(endpoint.fingerprintKey);

  void requestShutdown() => _send(const ShutdownMessage());

  @override
  void dispose() {
    _disposed = true;
    _closing = true;
    _reconnectTimer?.cancel();
    _subscription?.cancel();
    _channel?.close();
    super.dispose();
  }
}
