/// Mirrors `silo_core::protocol`: the wire protocol between the interactive
/// frontend (WebSocket server in the harness) and client applications.
///
/// Messages are JSON in WebSocket text frames. `ClientMessage` and
/// `ServerMessage` are internally tagged with `type`; `AuthRequest` is
/// internally tagged with `method` and flattened into the `authenticate`
/// client message.
library;

import 'event.dart';
import 'sandbox.dart';
import 'types.dart';

/// Mirrors `silo_core::protocol::AuthRequest`.
sealed class AuthRequest {
  const AuthRequest();

  factory AuthRequest.fromJson(Map<String, dynamic> json) {
    final method = json['method'] as String?;
    switch (method) {
      case 'local_token':
        return LocalTokenAuth(token: json['token'] as String);
      case 'pair':
        return PairAuth(
          code: json['code'] as String,
          publicKeyB64: json['public_key_b64'] as String,
          clientName: json['client_name'] as String,
        );
      case 'challenge':
        return ChallengeAuth(keyId: json['key_id'] as String);
      case 'signature':
        return SignatureAuth(
          keyId: json['key_id'] as String,
          signatureB64: json['signature_b64'] as String,
        );
    }
    throw FormatException('unknown auth method: $method');
  }

  /// The auth fields plus the `method` tag, ready to be flattened into the
  /// enclosing `authenticate` message.
  Map<String, dynamic> toJson();
}

class LocalTokenAuth extends AuthRequest {
  const LocalTokenAuth({required this.token});

  final String token;

  @override
  Map<String, dynamic> toJson() => {'method': 'local_token', 'token': token};
}

class PairAuth extends AuthRequest {
  const PairAuth({
    required this.code,
    required this.publicKeyB64,
    required this.clientName,
  });

  final String code;
  final String publicKeyB64;
  final String clientName;

  @override
  Map<String, dynamic> toJson() => {
        'method': 'pair',
        'code': code,
        'public_key_b64': publicKeyB64,
        'client_name': clientName,
      };
}

class ChallengeAuth extends AuthRequest {
  const ChallengeAuth({required this.keyId});

  final String keyId;

  @override
  Map<String, dynamic> toJson() => {'method': 'challenge', 'key_id': keyId};
}

class SignatureAuth extends AuthRequest {
  const SignatureAuth({required this.keyId, required this.signatureB64});

  final String keyId;
  final String signatureB64;

  @override
  Map<String, dynamic> toJson() => {
        'method': 'signature',
        'key_id': keyId,
        'signature_b64': signatureB64,
      };
}

/// Mirrors `silo_core::protocol::ClientMessage`.
sealed class ClientMessage {
  const ClientMessage();

  factory ClientMessage.fromJson(Map<String, dynamic> json) {
    final type = json['type'] as String?;
    switch (type) {
      case 'authenticate':
        return AuthenticateMessage(auth: AuthRequest.fromJson(json));
      case 'prompt':
        return PromptMessage(text: json['text'] as String);
      case 'answer_question':
        return AnswerQuestionMessage(
          questionId: json['question_id'] as String,
          answer: json['answer'] as String,
        );
      case 'upload_file':
        return UploadFileMessage(
          name: json['name'] as String,
          contentB64: json['content_b64'] as String,
        );
      case 'request_events':
        return RequestEventsMessage(fromSeq: json['from_seq'] as int);
      case 'request_access_report':
        return const RequestAccessReportMessage();
      case 'request_cost':
        return const RequestCostMessage();
      case 'request_pairing_code':
        return const RequestPairingCodeMessage();
      case 'interrupt':
        return const InterruptMessage();
      case 'shutdown':
        return const ShutdownMessage();
      case 'ping':
        return PingMessage(nonce: json['nonce'] as int);
    }
    throw FormatException('unknown client message type: $type');
  }

  Map<String, dynamic> toJson();
}

class AuthenticateMessage extends ClientMessage {
  const AuthenticateMessage({required this.auth});

  final AuthRequest auth;

  @override
  Map<String, dynamic> toJson() => {'type': 'authenticate', ...auth.toJson()};
}

class PromptMessage extends ClientMessage {
  const PromptMessage({required this.text});

  final String text;

  @override
  Map<String, dynamic> toJson() => {'type': 'prompt', 'text': text};
}

class AnswerQuestionMessage extends ClientMessage {
  const AnswerQuestionMessage({required this.questionId, required this.answer});

  final String questionId;
  final String answer;

  @override
  Map<String, dynamic> toJson() => {
        'type': 'answer_question',
        'question_id': questionId,
        'answer': answer,
      };
}

class UploadFileMessage extends ClientMessage {
  const UploadFileMessage({required this.name, required this.contentB64});

  final String name;
  final String contentB64;

  @override
  Map<String, dynamic> toJson() =>
      {'type': 'upload_file', 'name': name, 'content_b64': contentB64};
}

class RequestEventsMessage extends ClientMessage {
  const RequestEventsMessage({required this.fromSeq});

  final int fromSeq;

  @override
  Map<String, dynamic> toJson() =>
      {'type': 'request_events', 'from_seq': fromSeq};
}

class RequestAccessReportMessage extends ClientMessage {
  const RequestAccessReportMessage();

  @override
  Map<String, dynamic> toJson() => {'type': 'request_access_report'};
}

class RequestCostMessage extends ClientMessage {
  const RequestCostMessage();

  @override
  Map<String, dynamic> toJson() => {'type': 'request_cost'};
}

class RequestPairingCodeMessage extends ClientMessage {
  const RequestPairingCodeMessage();

  @override
  Map<String, dynamic> toJson() => {'type': 'request_pairing_code'};
}

class InterruptMessage extends ClientMessage {
  const InterruptMessage();

  @override
  Map<String, dynamic> toJson() => {'type': 'interrupt'};
}

class ShutdownMessage extends ClientMessage {
  const ShutdownMessage();

  @override
  Map<String, dynamic> toJson() => {'type': 'shutdown'};
}

class PingMessage extends ClientMessage {
  const PingMessage({required this.nonce});

  final int nonce;

  @override
  Map<String, dynamic> toJson() => {'type': 'ping', 'nonce': nonce};
}

/// Mirrors `silo_core::protocol::CostEntry`.
class CostEntry {
  const CostEntry({
    required this.backend,
    required this.usage,
    required this.quota,
  });

  final String backend;
  final UsageSnapshot usage;
  final QuotaConfig quota;

  factory CostEntry.fromJson(Map<String, dynamic> json) => CostEntry(
        backend: json['backend'] as String,
        usage: UsageSnapshot.fromJson(json['usage'] as Map<String, dynamic>),
        quota: QuotaConfig.fromJson(json['quota'] as Map<String, dynamic>),
      );

  Map<String, dynamic> toJson() => {
        'backend': backend,
        'usage': usage.toJson(),
        'quota': quota.toJson(),
      };
}

/// Mirrors `silo_core::protocol::ServerMessage`.
sealed class ServerMessage {
  const ServerMessage();

  factory ServerMessage.fromJson(Map<String, dynamic> json) {
    final type = json['type'] as String?;
    switch (type) {
      case 'hello':
        return HelloMessage(
          harnessId: json['harness_id'] as String,
          protocolVersion: json['protocol_version'] as int,
        );
      case 'auth_challenge':
        return AuthChallengeMessage(
          challengeB64: json['challenge_b64'] as String,
        );
      case 'auth_ok':
        return AuthOkMessage(
          clientId: json['client_id'] as String,
          keyId: json['key_id'] as String?,
          nextSeq: json['next_seq'] as int,
        );
      case 'auth_error':
        return AuthErrorMessage(message: json['message'] as String);
      case 'event':
        return EventMessage(
          event: Event.fromJson(json['event'] as Map<String, dynamic>),
        );
      case 'events':
        return EventsMessage(
          events: (json['events'] as List<dynamic>)
              .map((e) => Event.fromJson(e as Map<String, dynamic>))
              .toList(),
        );
      case 'access_report':
        return AccessReportMessage(
          report: AccessReport.fromJson(json['report'] as Map<String, dynamic>),
        );
      case 'cost':
        return CostMessage(
          entries: (json['entries'] as List<dynamic>)
              .map((e) => CostEntry.fromJson(e as Map<String, dynamic>))
              .toList(),
        );
      case 'pairing_code':
        return PairingCodeMessage(
          code: json['code'] as String,
          expiresInSecs: json['expires_in_secs'] as int,
        );
      case 'pong':
        return PongMessage(nonce: json['nonce'] as int);
      case 'error':
        return ErrorMessage(message: json['message'] as String);
      case 'shutting_down':
        return ShuttingDownMessage(message: json['message'] as String?);
    }
    throw FormatException('unknown server message type: $type');
  }

  Map<String, dynamic> toJson();
}

class HelloMessage extends ServerMessage {
  const HelloMessage({required this.harnessId, required this.protocolVersion});

  final String harnessId;
  final int protocolVersion;

  @override
  Map<String, dynamic> toJson() => {
        'type': 'hello',
        'harness_id': harnessId,
        'protocol_version': protocolVersion,
      };
}

class AuthChallengeMessage extends ServerMessage {
  const AuthChallengeMessage({required this.challengeB64});

  final String challengeB64;

  @override
  Map<String, dynamic> toJson() =>
      {'type': 'auth_challenge', 'challenge_b64': challengeB64};
}

class AuthOkMessage extends ServerMessage {
  const AuthOkMessage({
    required this.clientId,
    this.keyId,
    required this.nextSeq,
  });

  final String clientId;
  final String? keyId;
  final int nextSeq;

  @override
  Map<String, dynamic> toJson() => {
        'type': 'auth_ok',
        'client_id': clientId,
        if (keyId != null) 'key_id': keyId,
        'next_seq': nextSeq,
      };
}

class AuthErrorMessage extends ServerMessage {
  const AuthErrorMessage({required this.message});

  final String message;

  @override
  Map<String, dynamic> toJson() => {'type': 'auth_error', 'message': message};
}

class EventMessage extends ServerMessage {
  const EventMessage({required this.event});

  final Event event;

  @override
  Map<String, dynamic> toJson() => {'type': 'event', 'event': event.toJson()};
}

class EventsMessage extends ServerMessage {
  const EventsMessage({required this.events});

  final List<Event> events;

  @override
  Map<String, dynamic> toJson() => {
        'type': 'events',
        'events': events.map((e) => e.toJson()).toList(),
      };
}

class AccessReportMessage extends ServerMessage {
  const AccessReportMessage({required this.report});

  final AccessReport report;

  @override
  Map<String, dynamic> toJson() =>
      {'type': 'access_report', 'report': report.toJson()};
}

class CostMessage extends ServerMessage {
  const CostMessage({required this.entries});

  final List<CostEntry> entries;

  @override
  Map<String, dynamic> toJson() => {
        'type': 'cost',
        'entries': entries.map((e) => e.toJson()).toList(),
      };
}

class PairingCodeMessage extends ServerMessage {
  const PairingCodeMessage({required this.code, required this.expiresInSecs});

  final String code;
  final int expiresInSecs;

  @override
  Map<String, dynamic> toJson() => {
        'type': 'pairing_code',
        'code': code,
        'expires_in_secs': expiresInSecs,
      };
}

class PongMessage extends ServerMessage {
  const PongMessage({required this.nonce});

  final int nonce;

  @override
  Map<String, dynamic> toJson() => {'type': 'pong', 'nonce': nonce};
}

class ErrorMessage extends ServerMessage {
  const ErrorMessage({required this.message});

  final String message;

  @override
  Map<String, dynamic> toJson() => {'type': 'error', 'message': message};
}

class ShuttingDownMessage extends ServerMessage {
  const ShuttingDownMessage({this.message});

  final String? message;

  @override
  Map<String, dynamic> toJson() =>
      {'type': 'shutting_down', if (message != null) 'message': message};
}

/// Mirrors `silo_core::protocol::RunInfo`: connection details written to the
/// run file for local clients.
class RunInfo {
  const RunInfo({
    required this.harnessId,
    required this.addr,
    required this.certFingerprintSha256,
    required this.localTokenPath,
    required this.pid,
    required this.workspace,
  });

  final String harnessId;
  final String addr;
  final String certFingerprintSha256;
  final String localTokenPath;
  final int pid;
  final String workspace;

  factory RunInfo.fromJson(Map<String, dynamic> json) => RunInfo(
        harnessId: json['harness_id'] as String,
        addr: json['addr'] as String,
        certFingerprintSha256: json['cert_fingerprint_sha256'] as String,
        localTokenPath: json['local_token_path'] as String,
        pid: json['pid'] as int,
        workspace: json['workspace'] as String,
      );

  Map<String, dynamic> toJson() => {
        'harness_id': harnessId,
        'addr': addr,
        'cert_fingerprint_sha256': certFingerprintSha256,
        'local_token_path': localTokenPath,
        'pid': pid,
        'workspace': workspace,
      };
}
