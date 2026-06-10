/// Mirrors `silo_core::sandbox::AccessReport`.
library;

class AccessReport {
  const AccessReport({
    required this.sandboxKind,
    required this.workspaceMount,
    required this.scratchDir,
    required this.readablePaths,
    required this.allowedDomains,
    required this.credentialDomains,
    required this.notes,
  });

  final String sandboxKind;
  final String workspaceMount;
  final String scratchDir;
  final List<String> readablePaths;
  final List<String> allowedDomains;
  final List<String> credentialDomains;
  final List<String> notes;

  factory AccessReport.fromJson(Map<String, dynamic> json) => AccessReport(
        sandboxKind: json['sandbox_kind'] as String,
        workspaceMount: json['workspace_mount'] as String,
        scratchDir: json['scratch_dir'] as String,
        readablePaths: _stringList(json['readable_paths']),
        allowedDomains: _stringList(json['allowed_domains']),
        credentialDomains: _stringList(json['credential_domains']),
        notes: _stringList(json['notes']),
      );

  Map<String, dynamic> toJson() => {
        'sandbox_kind': sandboxKind,
        'workspace_mount': workspaceMount,
        'scratch_dir': scratchDir,
        'readable_paths': readablePaths,
        'allowed_domains': allowedDomains,
        'credential_domains': credentialDomains,
        'notes': notes,
      };
}

List<String> _stringList(Object? value) =>
    (value as List<dynamic>? ?? const []).cast<String>();
