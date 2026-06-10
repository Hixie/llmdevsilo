/// Mirrors `silo_core::event`: the event stream shared by all frontends.
///
/// `Event` carries a sequence number, a timestamp, and a payload flattened
/// into the same JSON object. The payload is internally tagged with `kind`
/// (snake_case variant names). `FileOrigin` is internally tagged with
/// `origin` and flattened into the `file_shared` payload.
library;

import 'sandbox.dart';
import 'types.dart';

class Event {
  const Event({required this.seq, required this.time, required this.payload});

  final int seq;
  final Timestamp time;
  final EventPayload payload;

  factory Event.fromJson(Map<String, dynamic> json) => Event(
        seq: json['seq'] as int,
        time: Timestamp.fromJson(json['time'] as Map<String, dynamic>),
        payload: EventPayload.fromJson(json),
      );

  Map<String, dynamic> toJson() => {
        'seq': seq,
        'time': time.toJson(),
        ...payload.toJson(),
      };
}

/// Mirrors `silo_core::event::QuestionOption`.
class QuestionOption {
  const QuestionOption({required this.label, this.description = ''});

  final String label;
  final String description;

  factory QuestionOption.fromJson(Map<String, dynamic> json) => QuestionOption(
        label: json['label'] as String,
        description: json['description'] as String? ?? '',
      );

  Map<String, dynamic> toJson() => {'label': label, 'description': description};
}

/// Mirrors `silo_core::event::UserQuestion`.
class UserQuestion {
  const UserQuestion({
    required this.question,
    this.options = const [],
    this.multiSelect = false,
    this.allowFreeText = false,
  });

  final String question;
  final List<QuestionOption> options;
  final bool multiSelect;
  final bool allowFreeText;

  factory UserQuestion.fromJson(Map<String, dynamic> json) => UserQuestion(
        question: json['question'] as String,
        options: (json['options'] as List<dynamic>? ?? const [])
            .map((o) => QuestionOption.fromJson(o as Map<String, dynamic>))
            .toList(),
        multiSelect: json['multi_select'] as bool? ?? false,
        allowFreeText: json['allow_free_text'] as bool? ?? false,
      );

  Map<String, dynamic> toJson() => {
        'question': question,
        'options': options.map((o) => o.toJson()).toList(),
        'multi_select': multiSelect,
        'allow_free_text': allowFreeText,
      };
}

/// Mirrors `silo_core::event::FileOrigin`, internally tagged with `origin`.
sealed class FileOrigin {
  const FileOrigin();

  factory FileOrigin.fromJson(Map<String, dynamic> json) {
    final origin = json['origin'] as String?;
    switch (origin) {
      case 'client':
        return ClientOrigin(clientId: json['client_id'] as String);
      case 'llm':
        return LlmOrigin(agent: json['agent'] as String);
    }
    throw FormatException('unknown file origin: $origin');
  }

  Map<String, dynamic> toJson();
}

class ClientOrigin extends FileOrigin {
  const ClientOrigin({required this.clientId});

  final String clientId;

  @override
  Map<String, dynamic> toJson() => {'origin': 'client', 'client_id': clientId};
}

class LlmOrigin extends FileOrigin {
  const LlmOrigin({required this.agent});

  final String agent;

  @override
  Map<String, dynamic> toJson() => {'origin': 'llm', 'agent': agent};
}

/// Mirrors `silo_core::event::EventPayload`, internally tagged with `kind`.
sealed class EventPayload {
  const EventPayload();

  String get kind;

  factory EventPayload.fromJson(Map<String, dynamic> json) {
    final kind = json['kind'] as String?;
    switch (kind) {
      case 'harness_started':
        return HarnessStartedPayload(
          harnessId: json['harness_id'] as String,
          workspace: json['workspace'] as String,
          sandbox: json['sandbox'] as String,
          llm: json['llm'] as String,
        );
      case 'user_prompt':
        return UserPromptPayload(
          clientId: json['client_id'] as String?,
          clientName: json['client_name'] as String?,
          text: json['text'] as String,
        );
      case 'assistant_text':
        return AssistantTextPayload(
          agent: json['agent'] as String,
          text: json['text'] as String,
        );
      case 'tool_use':
        return ToolUsePayload(
          agent: json['agent'] as String,
          call: ToolCall.fromJson(json['call'] as Map<String, dynamic>),
        );
      case 'tool_result':
        return ToolResultPayload(
          agent: json['agent'] as String,
          toolUseId: json['tool_use_id'] as String,
          toolName: json['tool_name'] as String,
          output: ToolOutput.fromJson(json['output'] as Map<String, dynamic>),
        );
      case 'agent_spawned':
        return AgentSpawnedPayload(
          parent: json['parent'] as String,
          agent: json['agent'] as String,
          name: json['name'] as String?,
          prompt: json['prompt'] as String,
        );
      case 'agent_completed':
        return AgentCompletedPayload(
          agent: json['agent'] as String,
          result: json['result'] as String,
          isError: json['is_error'] as bool,
        );
      case 'question_asked':
        return QuestionAskedPayload(
          id: json['id'] as String,
          agent: json['agent'] as String,
          question:
              UserQuestion.fromJson(json['question'] as Map<String, dynamic>),
        );
      case 'question_answered':
        return QuestionAnsweredPayload(
          id: json['id'] as String,
          clientId: json['client_id'] as String?,
          answer: json['answer'] as String,
        );
      case 'file_shared':
        return FileSharedPayload(
          name: json['name'] as String,
          contentB64: json['content_b64'] as String,
          origin: FileOrigin.fromJson(json),
        );
      case 'cost_report':
        return CostReportPayload(
          backend: json['backend'] as String,
          usage: UsageSnapshot.fromJson(json['usage'] as Map<String, dynamic>),
          quota: QuotaConfig.fromJson(json['quota'] as Map<String, dynamic>),
        );
      case 'turn_complete':
        return TurnCompletePayload(
          agent: json['agent'] as String,
          stopReason: StopReason.fromJson(json['stop_reason']),
        );
      case 'awaiting_input':
        return const AwaitingInputPayload();
      case 'interrupted':
        return InterruptedPayload(agent: json['agent'] as String);
      case 'access_report_updated':
        return AccessReportUpdatedPayload(
          report: AccessReport.fromJson(json['report'] as Map<String, dynamic>),
        );
      case 'error':
        return ErrorPayload(
          context: json['context'] as String,
          message: json['message'] as String,
        );
      case 'shutdown':
        return ShutdownPayload(message: json['message'] as String?);
    }
    // Payloads from newer servers are kept verbatim so the transcript can
    // still display something and re-encoding is lossless.
    return UnknownPayload(raw: Map<String, dynamic>.from(json)
      ..remove('seq')
      ..remove('time'));
  }

  /// The payload fields plus the `kind` tag, ready to be flattened into the
  /// enclosing event object.
  Map<String, dynamic> toJson();
}

class HarnessStartedPayload extends EventPayload {
  const HarnessStartedPayload({
    required this.harnessId,
    required this.workspace,
    required this.sandbox,
    required this.llm,
  });

  final String harnessId;
  final String workspace;
  final String sandbox;
  final String llm;

  @override
  String get kind => 'harness_started';

  @override
  Map<String, dynamic> toJson() => {
        'kind': kind,
        'harness_id': harnessId,
        'workspace': workspace,
        'sandbox': sandbox,
        'llm': llm,
      };
}

class UserPromptPayload extends EventPayload {
  const UserPromptPayload({this.clientId, this.clientName, required this.text});

  final String? clientId;

  /// Human-readable name of the client that sent the prompt. Absent when
  /// the harness does not know one.
  final String? clientName;
  final String text;

  @override
  String get kind => 'user_prompt';

  @override
  Map<String, dynamic> toJson() => {
        'kind': kind,
        if (clientId != null) 'client_id': clientId,
        if (clientName != null) 'client_name': clientName,
        'text': text,
      };
}

class AssistantTextPayload extends EventPayload {
  const AssistantTextPayload({required this.agent, required this.text});

  final String agent;
  final String text;

  @override
  String get kind => 'assistant_text';

  @override
  Map<String, dynamic> toJson() => {'kind': kind, 'agent': agent, 'text': text};
}

class ToolUsePayload extends EventPayload {
  const ToolUsePayload({required this.agent, required this.call});

  final String agent;
  final ToolCall call;

  @override
  String get kind => 'tool_use';

  @override
  Map<String, dynamic> toJson() =>
      {'kind': kind, 'agent': agent, 'call': call.toJson()};
}

class ToolResultPayload extends EventPayload {
  const ToolResultPayload({
    required this.agent,
    required this.toolUseId,
    required this.toolName,
    required this.output,
  });

  final String agent;
  final String toolUseId;
  final String toolName;
  final ToolOutput output;

  @override
  String get kind => 'tool_result';

  @override
  Map<String, dynamic> toJson() => {
        'kind': kind,
        'agent': agent,
        'tool_use_id': toolUseId,
        'tool_name': toolName,
        'output': output.toJson(),
      };
}

class AgentSpawnedPayload extends EventPayload {
  const AgentSpawnedPayload({
    required this.parent,
    required this.agent,
    this.name,
    required this.prompt,
  });

  final String parent;
  final String agent;

  /// Name given to the subagent by the model. Absent when none was given.
  final String? name;
  final String prompt;

  @override
  String get kind => 'agent_spawned';

  @override
  Map<String, dynamic> toJson() => {
        'kind': kind,
        'parent': parent,
        'agent': agent,
        if (name != null) 'name': name,
        'prompt': prompt,
      };
}

class AgentCompletedPayload extends EventPayload {
  const AgentCompletedPayload({
    required this.agent,
    required this.result,
    required this.isError,
  });

  final String agent;
  final String result;
  final bool isError;

  @override
  String get kind => 'agent_completed';

  @override
  Map<String, dynamic> toJson() =>
      {'kind': kind, 'agent': agent, 'result': result, 'is_error': isError};
}

class QuestionAskedPayload extends EventPayload {
  const QuestionAskedPayload({
    required this.id,
    required this.agent,
    required this.question,
  });

  final String id;
  final String agent;
  final UserQuestion question;

  @override
  String get kind => 'question_asked';

  @override
  Map<String, dynamic> toJson() =>
      {'kind': kind, 'id': id, 'agent': agent, 'question': question.toJson()};
}

class QuestionAnsweredPayload extends EventPayload {
  const QuestionAnsweredPayload({
    required this.id,
    this.clientId,
    required this.answer,
  });

  final String id;
  final String? clientId;
  final String answer;

  @override
  String get kind => 'question_answered';

  @override
  Map<String, dynamic> toJson() => {
        'kind': kind,
        'id': id,
        if (clientId != null) 'client_id': clientId,
        'answer': answer,
      };
}

class FileSharedPayload extends EventPayload {
  const FileSharedPayload({
    required this.name,
    required this.contentB64,
    required this.origin,
  });

  final String name;
  final String contentB64;
  final FileOrigin origin;

  @override
  String get kind => 'file_shared';

  @override
  Map<String, dynamic> toJson() => {
        'kind': kind,
        'name': name,
        'content_b64': contentB64,
        ...origin.toJson(),
      };
}

class CostReportPayload extends EventPayload {
  const CostReportPayload({
    required this.backend,
    required this.usage,
    required this.quota,
  });

  final String backend;
  final UsageSnapshot usage;
  final QuotaConfig quota;

  @override
  String get kind => 'cost_report';

  @override
  Map<String, dynamic> toJson() => {
        'kind': kind,
        'backend': backend,
        'usage': usage.toJson(),
        'quota': quota.toJson(),
      };
}

class TurnCompletePayload extends EventPayload {
  const TurnCompletePayload({required this.agent, required this.stopReason});

  final String agent;
  final StopReason stopReason;

  @override
  String get kind => 'turn_complete';

  @override
  Map<String, dynamic> toJson() =>
      {'kind': kind, 'agent': agent, 'stop_reason': stopReason.toJson()};
}

class AwaitingInputPayload extends EventPayload {
  const AwaitingInputPayload();

  @override
  String get kind => 'awaiting_input';

  @override
  Map<String, dynamic> toJson() => {'kind': kind};
}

class InterruptedPayload extends EventPayload {
  const InterruptedPayload({required this.agent});

  final String agent;

  @override
  String get kind => 'interrupted';

  @override
  Map<String, dynamic> toJson() => {'kind': kind, 'agent': agent};
}

class AccessReportUpdatedPayload extends EventPayload {
  const AccessReportUpdatedPayload({required this.report});

  final AccessReport report;

  @override
  String get kind => 'access_report_updated';

  @override
  Map<String, dynamic> toJson() => {'kind': kind, 'report': report.toJson()};
}

class ErrorPayload extends EventPayload {
  const ErrorPayload({required this.context, required this.message});

  final String context;
  final String message;

  @override
  String get kind => 'error';

  @override
  Map<String, dynamic> toJson() =>
      {'kind': kind, 'context': context, 'message': message};
}

class ShutdownPayload extends EventPayload {
  const ShutdownPayload({this.message});

  final String? message;

  @override
  String get kind => 'shutdown';

  @override
  Map<String, dynamic> toJson() =>
      {'kind': kind, if (message != null) 'message': message};
}

/// Payload with a `kind` this client does not know. Retains the raw fields.
class UnknownPayload extends EventPayload {
  const UnknownPayload({required this.raw});

  final Map<String, dynamic> raw;

  @override
  String get kind => raw['kind'] as String? ?? 'unknown';

  @override
  Map<String, dynamic> toJson() => raw;
}
