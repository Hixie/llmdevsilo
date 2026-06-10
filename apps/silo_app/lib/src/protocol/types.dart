/// Shared protocol value types mirroring `silo-core`'s `clock.rs`,
/// `tool.rs`, `conversation.rs`, and `cost.rs` JSON shapes.
library;

/// Mirrors `silo_core::clock::Timestamp`. `wall_ms` is omitted from JSON
/// when absent.
class Timestamp {
  const Timestamp({required this.logical, this.wallMs});

  final int logical;
  final int? wallMs;

  factory Timestamp.fromJson(Map<String, dynamic> json) => Timestamp(
        logical: json['logical'] as int,
        wallMs: json['wall_ms'] as int?,
      );

  Map<String, dynamic> toJson() => {
        'logical': logical,
        if (wallMs != null) 'wall_ms': wallMs,
      };
}

/// Mirrors `silo_core::tool::ToolCall`.
class ToolCall {
  const ToolCall({required this.id, required this.name, required this.input});

  final String id;
  final String name;

  /// Arbitrary JSON value (the tool input).
  final Object? input;

  factory ToolCall.fromJson(Map<String, dynamic> json) => ToolCall(
        id: json['id'] as String,
        name: json['name'] as String,
        input: json['input'],
      );

  Map<String, dynamic> toJson() => {'id': id, 'name': name, 'input': input};
}

/// Mirrors `silo_core::tool::ToolOutput`.
class ToolOutput {
  const ToolOutput({required this.content, required this.isError});

  final String content;
  final bool isError;

  factory ToolOutput.fromJson(Map<String, dynamic> json) => ToolOutput(
        content: json['content'] as String,
        isError: json['is_error'] as bool,
      );

  Map<String, dynamic> toJson() => {'content': content, 'is_error': isError};
}

/// Mirrors `silo_core::conversation::StopReason`, an externally tagged Rust
/// enum: the unit variants serialize as the strings `"end_turn"`,
/// `"tool_use"`, and `"max_tokens"`; the `Other(String)` variant serializes
/// as the object `{"other": "..."}`.
class StopReason {
  const StopReason._(this.kind, this.other);

  final String kind;

  /// Detail string when [kind] is `other`.
  final String? other;

  static const endTurn = StopReason._('end_turn', null);
  static const toolUse = StopReason._('tool_use', null);
  static const maxTokens = StopReason._('max_tokens', null);

  factory StopReason.other(String detail) => StopReason._('other', detail);

  factory StopReason.fromJson(Object? json) {
    if (json is String) {
      switch (json) {
        case 'end_turn':
          return endTurn;
        case 'tool_use':
          return toolUse;
        case 'max_tokens':
          return maxTokens;
      }
      throw FormatException('unknown stop reason: $json');
    }
    if (json is Map<String, dynamic> && json['other'] is String) {
      return StopReason.other(json['other'] as String);
    }
    throw FormatException('unknown stop reason shape: $json');
  }

  Object toJson() => other == null ? kind : {'other': other};

  String get label => other ?? kind;
}

/// Mirrors `silo_core::cost::UsageSnapshot`.
class UsageSnapshot {
  const UsageSnapshot({
    required this.inputTokens,
    required this.outputTokens,
    required this.usd,
  });

  final int inputTokens;
  final int outputTokens;
  final double usd;

  int get totalTokens => inputTokens + outputTokens;

  factory UsageSnapshot.fromJson(Map<String, dynamic> json) => UsageSnapshot(
        inputTokens: json['input_tokens'] as int,
        outputTokens: json['output_tokens'] as int,
        usd: (json['usd'] as num).toDouble(),
      );

  Map<String, dynamic> toJson() => {
        'input_tokens': inputTokens,
        'output_tokens': outputTokens,
        'usd': usd,
      };
}

/// Mirrors `silo_core::cost::QuotaConfig`. Both fields are omitted from
/// JSON when absent.
class QuotaConfig {
  const QuotaConfig({this.maxTotalTokens, this.maxUsd});

  final int? maxTotalTokens;
  final double? maxUsd;

  factory QuotaConfig.fromJson(Map<String, dynamic> json) => QuotaConfig(
        maxTotalTokens: json['max_total_tokens'] as int?,
        maxUsd: (json['max_usd'] as num?)?.toDouble(),
      );

  Map<String, dynamic> toJson() => {
        if (maxTotalTokens != null) 'max_total_tokens': maxTotalTokens,
        if (maxUsd != null) 'max_usd': maxUsd,
      };
}
