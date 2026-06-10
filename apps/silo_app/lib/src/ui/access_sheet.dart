/// Bottom sheet rendering the sandbox [AccessReport].
library;

import 'package:flutter/material.dart';

import '../protocol/sandbox.dart';
import 'theme.dart';

void showAccessSheet(BuildContext context, AccessReport? report) {
  showModalBottomSheet<void>(
    context: context,
    showDragHandle: true,
    isScrollControlled: true,
    builder: (context) => DraggableScrollableSheet(
      expand: false,
      initialChildSize: 0.6,
      maxChildSize: 0.95,
      builder: (context, controller) =>
          _AccessSheetBody(report: report, controller: controller),
    ),
  );
}

class _AccessSheetBody extends StatelessWidget {
  const _AccessSheetBody({required this.report, required this.controller});

  final AccessReport? report;
  final ScrollController controller;

  @override
  Widget build(BuildContext context) {
    final report = this.report;
    if (report == null) {
      return const Center(
        child: Padding(
          padding: EdgeInsets.all(32),
          child: Text('No access report received yet.'),
        ),
      );
    }
    final theme = Theme.of(context);
    return ListView(
      controller: controller,
      padding: const EdgeInsets.fromLTRB(20, 0, 20, 24),
      children: [
        Text('Sandbox access', style: theme.textTheme.titleLarge),
        const SizedBox(height: 4),
        Text(
          'Everything the model can reach.',
          style: theme.textTheme.bodySmall
              ?.copyWith(color: theme.colorScheme.outline),
        ),
        const SizedBox(height: 16),
        _section(context, Icons.shield_outlined, 'Sandbox',
            [report.sandboxKind]),
        _section(context, Icons.folder_outlined, 'Workspace (read/write)',
            [report.workspaceMount]),
        _section(context, Icons.folder_special_outlined, 'Scratch space',
            [report.scratchDir]),
        _section(context, Icons.menu_book_outlined, 'Readable host paths',
            report.readablePaths),
        _section(context, Icons.public, 'Allowed domains',
            report.allowedDomains),
        _section(context, Icons.key_outlined, 'Credential-injected domains',
            report.credentialDomains),
        if (report.notes.isNotEmpty)
          _section(context, Icons.notes_outlined, 'Notes', report.notes),
      ],
    );
  }

  Widget _section(
    BuildContext context,
    IconData icon,
    String title,
    List<String> items,
  ) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.only(bottom: 16),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Icon(icon, size: 18, color: theme.colorScheme.primary),
              const SizedBox(width: 8),
              Text(title, style: theme.textTheme.titleSmall),
            ],
          ),
          const SizedBox(height: 6),
          if (items.isEmpty)
            Padding(
              padding: const EdgeInsets.only(left: 26),
              child: Text(
                'None',
                style: theme.textTheme.bodySmall
                    ?.copyWith(color: theme.colorScheme.outline),
              ),
            )
          else
            for (final item in items)
              Padding(
                padding: const EdgeInsets.only(left: 26, bottom: 2),
                child: SelectableText(
                  item,
                  style: const TextStyle(
                    fontFamily: monoFontFamily,
                    fontFamilyFallback: ['Menlo', 'Courier New'],
                    fontSize: 13,
                  ),
                ),
              ),
        ],
      ),
    );
  }
}
