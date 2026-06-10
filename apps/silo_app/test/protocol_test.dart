// Codec tests against hand-written JSON fixtures matching the serde output
// of silo-core's protocol.rs, event.rs, and sandbox.rs.

import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/protocol/event.dart';
import 'package:silo_app/src/protocol/protocol.dart';
import 'package:silo_app/src/protocol/sandbox.dart';
import 'package:silo_app/src/protocol/types.dart';

Map<String, dynamic> eventFixture(int seq, Map<String, dynamic> payload) => {
      'seq': seq,
      'time': {'logical': seq},
      ...payload,
    };

void expectEventRoundTrip(Map<String, dynamic> fixture) {
  final event = Event.fromJson(fixture);
  expect(event.toJson(), equals(fixture));
}

void main() {
  group('EventPayload variants round-trip', () {
    test('harness_started', () {
      final fixture = eventFixture(0, {
        'kind': 'harness_started',
        'harness_id': 'h-1',
        'workspace': '/work',
        'sandbox': 'mock',
        'llm': 'anthropic',
      });
      expectEventRoundTrip(fixture);
      final event = Event.fromJson(fixture);
      expect(event.payload, isA<HarnessStartedPayload>());
      expect((event.payload as HarnessStartedPayload).llm, 'anthropic');
    });

    test('user_prompt with client_id', () {
      final fixture = eventFixture(1, {
        'kind': 'user_prompt',
        'client_id': 'client-1',
        'text': 'hello',
      });
      expectEventRoundTrip(fixture);
    });

    test('user_prompt without client_id (skip_serializing_if)', () {
      final fixture = eventFixture(1, {
        'kind': 'user_prompt',
        'text': 'hello',
      });
      final event = Event.fromJson(fixture);
      expect((event.payload as UserPromptPayload).clientId, isNull);
      // Re-encoding omits the absent field, matching serde.
      expect(event.toJson(), equals(fixture));
    });

    test('user_prompt with client_name', () {
      final fixture = eventFixture(1, {
        'kind': 'user_prompt',
        'client_id': 'client-1',
        'client_name': "Ian's phone",
        'text': 'hello',
      });
      expectEventRoundTrip(fixture);
      final event = Event.fromJson(fixture);
      expect((event.payload as UserPromptPayload).clientName, "Ian's phone");
    });

    test('user_prompt without client_name omits it on re-encode', () {
      final fixture = eventFixture(1, {
        'kind': 'user_prompt',
        'client_id': 'client-1',
        'text': 'hello',
      });
      final event = Event.fromJson(fixture);
      expect((event.payload as UserPromptPayload).clientName, isNull);
      expect(event.toJson(), equals(fixture));
    });

    test('assistant_text', () {
      final fixture = eventFixture(2, {
        'kind': 'assistant_text',
        'agent': 'agent-0',
        'text': 'Hi there.',
      });
      expectEventRoundTrip(fixture);
    });

    test('tool_use', () {
      final fixture = eventFixture(3, {
        'kind': 'tool_use',
        'agent': 'agent-0',
        'call': {
          'id': 'toolu_1',
          'name': 'Bash',
          'input': {'command': 'ls', 'timeout': 5},
        },
      });
      expectEventRoundTrip(fixture);
      final event = Event.fromJson(fixture);
      final call = (event.payload as ToolUsePayload).call;
      expect(call.name, 'Bash');
      expect((call.input as Map<String, dynamic>)['command'], 'ls');
    });

    test('tool_result', () {
      final fixture = eventFixture(4, {
        'kind': 'tool_result',
        'agent': 'agent-0',
        'tool_use_id': 'toolu_1',
        'tool_name': 'Bash',
        'output': {'content': 'README.md', 'is_error': false},
      });
      expectEventRoundTrip(fixture);
    });

    test('agent_spawned', () {
      final fixture = eventFixture(5, {
        'kind': 'agent_spawned',
        'parent': 'agent-0',
        'agent': 'agent-1',
        'prompt': 'investigate the tests',
      });
      expectEventRoundTrip(fixture);
      // Re-encoding omits the absent name, matching serde.
      final event = Event.fromJson(fixture);
      expect((event.payload as AgentSpawnedPayload).name, isNull);
    });

    test('agent_spawned with name', () {
      final fixture = eventFixture(5, {
        'kind': 'agent_spawned',
        'parent': 'agent-0',
        'agent': 'agent-1',
        'name': 'refactor tests',
        'prompt': 'investigate the tests',
      });
      expectEventRoundTrip(fixture);
      final event = Event.fromJson(fixture);
      expect((event.payload as AgentSpawnedPayload).name, 'refactor tests');
    });

    test('agent_completed', () {
      final fixture = eventFixture(6, {
        'kind': 'agent_completed',
        'agent': 'agent-1',
        'result': 'done',
        'is_error': false,
      });
      expectEventRoundTrip(fixture);
    });

    test('question_asked with options and flags', () {
      final fixture = eventFixture(7, {
        'kind': 'question_asked',
        'id': 'q-1',
        'agent': 'agent-0',
        'question': {
          'question': 'Which color?',
          'options': [
            {'label': 'Red', 'description': 'warm'},
            {'label': 'Blue', 'description': ''},
          ],
          'multi_select': true,
          'allow_free_text': true,
        },
      });
      expectEventRoundTrip(fixture);
    });

    test('question_asked with serde defaults absent', () {
      // serde fills missing #[serde(default)] fields on deserialization.
      final fixture = eventFixture(7, {
        'kind': 'question_asked',
        'id': 'q-2',
        'agent': 'agent-0',
        'question': {'question': 'Proceed?'},
      });
      final event = Event.fromJson(fixture);
      final question = (event.payload as QuestionAskedPayload).question;
      expect(question.options, isEmpty);
      expect(question.multiSelect, isFalse);
      expect(question.allowFreeText, isFalse);
    });

    test('question_answered', () {
      final fixture = eventFixture(8, {
        'kind': 'question_answered',
        'id': 'q-1',
        'client_id': 'client-2',
        'answer': 'Red',
      });
      expectEventRoundTrip(fixture);
      final without = eventFixture(8, {
        'kind': 'question_answered',
        'id': 'q-1',
        'answer': 'Red',
      });
      expectEventRoundTrip(without);
    });

    test('file_shared from client (flattened origin tag)', () {
      final fixture = eventFixture(9, {
        'kind': 'file_shared',
        'name': 'notes.txt',
        'content_b64': 'aGVsbG8=',
        'origin': 'client',
        'client_id': 'client-1',
      });
      expectEventRoundTrip(fixture);
      final event = Event.fromJson(fixture);
      final origin = (event.payload as FileSharedPayload).origin;
      expect(origin, isA<ClientOrigin>());
    });

    test('file_shared from llm', () {
      final fixture = eventFixture(10, {
        'kind': 'file_shared',
        'name': 'report.pdf',
        'content_b64': 'AAEC',
        'origin': 'llm',
        'agent': 'agent-0',
      });
      expectEventRoundTrip(fixture);
      final event = Event.fromJson(fixture);
      final origin = (event.payload as FileSharedPayload).origin;
      expect(origin, isA<LlmOrigin>());
      expect((origin as LlmOrigin).agent, 'agent-0');
    });

    test('cost_report', () {
      final fixture = eventFixture(11, {
        'kind': 'cost_report',
        'backend': 'anthropic',
        'usage': {'input_tokens': 1200, 'output_tokens': 340, 'usd': 0.0125},
        'quota': {'max_total_tokens': 1000000, 'max_usd': 5.0},
      });
      expectEventRoundTrip(fixture);
    });

    test('cost_report with empty quota (skip_serializing_if)', () {
      final fixture = eventFixture(11, {
        'kind': 'cost_report',
        'backend': 'mock',
        'usage': {'input_tokens': 0, 'output_tokens': 0, 'usd': 0.0},
        'quota': <String, dynamic>{},
      });
      expectEventRoundTrip(fixture);
    });

    test('turn_complete with unit stop reason', () {
      final fixture = eventFixture(12, {
        'kind': 'turn_complete',
        'agent': 'agent-0',
        'stop_reason': 'end_turn',
      });
      expectEventRoundTrip(fixture);
      final event = Event.fromJson(fixture);
      expect((event.payload as TurnCompletePayload).stopReason,
          StopReason.endTurn);
    });

    test('turn_complete with Other stop reason (externally tagged)', () {
      final fixture = eventFixture(12, {
        'kind': 'turn_complete',
        'agent': 'agent-0',
        'stop_reason': {'other': 'content_filter'},
      });
      expectEventRoundTrip(fixture);
      final event = Event.fromJson(fixture);
      final reason = (event.payload as TurnCompletePayload).stopReason;
      expect(reason.other, 'content_filter');
    });

    test('awaiting_input (unit variant)', () {
      final fixture = eventFixture(13, {'kind': 'awaiting_input'});
      expectEventRoundTrip(fixture);
    });

    test('interrupted', () {
      final fixture = eventFixture(13, {
        'kind': 'interrupted',
        'agent': 'agent-0',
      });
      expectEventRoundTrip(fixture);
      final event = Event.fromJson(fixture);
      expect(event.payload, isA<InterruptedPayload>());
      expect((event.payload as InterruptedPayload).agent, 'agent-0');
      expect(event.payload.kind, 'interrupted');
    });

    test('access_report_updated', () {
      final fixture = eventFixture(14, {
        'kind': 'access_report_updated',
        'report': {
          'sandbox_kind': 'macos-sandbox-exec',
          'workspace_mount': '/workspace',
          'scratch_dir': '/scratch',
          'readable_paths': ['/usr/bin', '/usr/lib'],
          'allowed_domains': ['crates.io', 'github.com'],
          'credential_domains': ['github.com'],
          'notes': ['workspace lock is best-effort on this platform'],
        },
      });
      expectEventRoundTrip(fixture);
    });

    test('error', () {
      final fixture = eventFixture(15, {
        'kind': 'error',
        'context': 'llm',
        'message': 'quota exceeded',
      });
      expectEventRoundTrip(fixture);
    });

    test('shutdown with and without message', () {
      expectEventRoundTrip(eventFixture(16, {
        'kind': 'shutdown',
        'message': 'task complete',
      }));
      expectEventRoundTrip(eventFixture(16, {'kind': 'shutdown'}));
    });

    test('unknown kind is preserved verbatim', () {
      final fixture = eventFixture(17, {
        'kind': 'from_the_future',
        'payload': 42,
      });
      final event = Event.fromJson(fixture);
      expect(event.payload, isA<UnknownPayload>());
      expect(event.toJson(), equals(fixture));
    });

    test('timestamp with wall_ms round-trips', () {
      final fixture = {
        'seq': 3,
        'time': {'logical': 3, 'wall_ms': 1717000000000},
        'kind': 'awaiting_input',
      };
      expectEventRoundTrip(fixture);
    });
  });

  group('ClientMessage', () {
    void roundTrip(ClientMessage message, Map<String, dynamic> fixture) {
      expect(message.toJson(), equals(fixture));
      expect(ClientMessage.fromJson(fixture).toJson(), equals(fixture));
    }

    test('authenticate local_token (flattened method tag)', () {
      roundTrip(
        const AuthenticateMessage(auth: LocalTokenAuth(token: 'tok')),
        {'type': 'authenticate', 'method': 'local_token', 'token': 'tok'},
      );
    });

    test('authenticate pair', () {
      roundTrip(
        const AuthenticateMessage(
          auth: PairAuth(
            code: 'A1B2C3D4',
            publicKeyB64: 'cHVibGlj',
            clientName: 'phone',
          ),
        ),
        {
          'type': 'authenticate',
          'method': 'pair',
          'code': 'A1B2C3D4',
          'public_key_b64': 'cHVibGlj',
          'client_name': 'phone',
        },
      );
    });

    test('authenticate challenge', () {
      roundTrip(
        const AuthenticateMessage(auth: ChallengeAuth(keyId: 'key-1')),
        {'type': 'authenticate', 'method': 'challenge', 'key_id': 'key-1'},
      );
    });

    test('authenticate signature', () {
      roundTrip(
        const AuthenticateMessage(
          auth: SignatureAuth(keyId: 'key-1', signatureB64: 'c2ln'),
        ),
        {
          'type': 'authenticate',
          'method': 'signature',
          'key_id': 'key-1',
          'signature_b64': 'c2ln',
        },
      );
    });

    test('prompt', () {
      roundTrip(
        const PromptMessage(text: 'do the thing'),
        {'type': 'prompt', 'text': 'do the thing'},
      );
    });

    test('answer_question', () {
      roundTrip(
        const AnswerQuestionMessage(questionId: 'q-1', answer: 'Red'),
        {'type': 'answer_question', 'question_id': 'q-1', 'answer': 'Red'},
      );
    });

    test('upload_file', () {
      roundTrip(
        const UploadFileMessage(name: 'a.txt', contentB64: 'aGk='),
        {'type': 'upload_file', 'name': 'a.txt', 'content_b64': 'aGk='},
      );
    });

    test('request_events', () {
      roundTrip(
        const RequestEventsMessage(fromSeq: 42),
        {'type': 'request_events', 'from_seq': 42},
      );
    });

    test('unit variants', () {
      roundTrip(const RequestAccessReportMessage(),
          {'type': 'request_access_report'});
      roundTrip(const RequestCostMessage(), {'type': 'request_cost'});
      roundTrip(const RequestPairingCodeMessage(),
          {'type': 'request_pairing_code'});
      roundTrip(const InterruptMessage(), {'type': 'interrupt'});
      roundTrip(const ShutdownMessage(), {'type': 'shutdown'});
    });

    test('ping', () {
      roundTrip(const PingMessage(nonce: 7), {'type': 'ping', 'nonce': 7});
    });
  });

  group('ServerMessage', () {
    void roundTrip(Map<String, dynamic> fixture) {
      expect(ServerMessage.fromJson(fixture).toJson(), equals(fixture));
    }

    test('hello', () {
      roundTrip({'type': 'hello', 'harness_id': 'h-1', 'protocol_version': 1});
    });

    test('auth_challenge', () {
      roundTrip({'type': 'auth_challenge', 'challenge_b64': 'Y2hhbGxlbmdl'});
    });

    test('auth_ok with and without key_id', () {
      roundTrip({
        'type': 'auth_ok',
        'client_id': 'client-1',
        'key_id': 'key-1',
        'next_seq': 10,
      });
      roundTrip({'type': 'auth_ok', 'client_id': 'client-1', 'next_seq': 0});
    });

    test('auth_error', () {
      roundTrip({'type': 'auth_error', 'message': 'bad token'});
    });

    test('event', () {
      roundTrip({
        'type': 'event',
        'event': eventFixture(5, {
          'kind': 'assistant_text',
          'agent': 'agent-0',
          'text': 'hi',
        }),
      });
    });

    test('events backlog', () {
      roundTrip({
        'type': 'events',
        'events': [
          eventFixture(0, {'kind': 'awaiting_input'}),
          eventFixture(1, {
            'kind': 'user_prompt',
            'client_id': 'c',
            'text': 'x',
          }),
        ],
      });
    });

    test('access_report', () {
      roundTrip({
        'type': 'access_report',
        'report': {
          'sandbox_kind': 'gvisor',
          'workspace_mount': '/workspace',
          'scratch_dir': '/scratch',
          'readable_paths': <String>[],
          'allowed_domains': ['docs.rs'],
          'credential_domains': <String>[],
          'notes': <String>[],
        },
      });
    });

    test('cost', () {
      roundTrip({
        'type': 'cost',
        'entries': [
          {
            'backend': 'anthropic',
            'usage': {'input_tokens': 10, 'output_tokens': 20, 'usd': 0.001},
            'quota': {'max_usd': 2.5},
          },
        ],
      });
    });

    test('pairing_code', () {
      roundTrip({
        'type': 'pairing_code',
        'code': 'A1B2C3D4',
        'expires_in_secs': 120,
      });
    });

    test('pong', () {
      roundTrip({'type': 'pong', 'nonce': 9});
    });

    test('error', () {
      roundTrip({'type': 'error', 'message': 'nope'});
    });

    test('shutting_down with and without message', () {
      roundTrip({'type': 'shutting_down', 'message': 'bye'});
      roundTrip({'type': 'shutting_down'});
    });
  });

  group('RunInfo', () {
    test('round-trips', () {
      final fixture = {
        'harness_id': 'h-1',
        'addr': '127.0.0.1:7777',
        'cert_fingerprint_sha256': 'ab' * 32,
        'local_token_path': '/home/u/.llmdevsilo/harness/h-1/local-token',
        'pid': 4242,
        'workspace': '/home/u/project',
      };
      expect(RunInfo.fromJson(fixture).toJson(), equals(fixture));
    });
  });

  group('AccessReport', () {
    test('round-trips', () {
      final fixture = {
        'sandbox_kind': 'mock',
        'workspace_mount': '/workspace',
        'scratch_dir': '/tmp/scratch',
        'readable_paths': ['/usr/bin'],
        'allowed_domains': ['example.com'],
        'credential_domains': ['example.com'],
        'notes': ['note'],
      };
      expect(AccessReport.fromJson(fixture).toJson(), equals(fixture));
    });
  });
}
