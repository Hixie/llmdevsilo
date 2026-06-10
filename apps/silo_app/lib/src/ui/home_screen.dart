/// The list of configured harnesses, with flows for adding new ones:
/// pairing with a remote harness, attaching to a local harness via its run
/// file, and (on macOS desktop) starting a new local harness.
library;

import 'package:file_selector/file_selector.dart';
import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import '../connection/endpoint.dart';
import '../connection/harness_connection.dart';
import '../connection/harness_registry.dart';
import '../connection/local_harness.dart' as local;
import '../connection/local_harness_options.dart';
import '../connection/silo_locator.dart';
import '../protocol/protocol.dart';
import 'chat_screen.dart';
import 'theme.dart';

class HomeScreen extends StatelessWidget {
  const HomeScreen({super.key});

  void _openChat(BuildContext context, HarnessConnection connection) {
    Navigator.of(context).push(
      MaterialPageRoute<void>(
        builder: (_) => ChatScreen(connection: connection),
      ),
    );
  }

  Future<void> _addPaired(BuildContext context) async {
    final registry = context.read<HarnessRegistry>();
    final result = await showDialog<_PairInput>(
      context: context,
      builder: (_) => const _PairDialog(),
    );
    if (result == null || !context.mounted) {
      return;
    }
    final connection = await registry.addPaired(
      name: result.name,
      url: result.url,
      pairingCode: result.code,
      fingerprintSha256: result.fingerprint,
    );
    if (context.mounted) {
      _openChat(context, connection);
    }
  }

  Future<void> _attachLocal(BuildContext context) async {
    final registry = context.read<HarnessRegistry>();
    final messenger = ScaffoldMessenger.of(context);
    final runs = await local.listLocalRuns();
    if (!context.mounted) {
      return;
    }
    if (runs.isEmpty) {
      messenger.showSnackBar(const SnackBar(
        content: Text('No running local harnesses found.'),
      ));
      return;
    }
    final run = await showDialog<RunInfo>(
      context: context,
      builder: (context) => SimpleDialog(
        title: const Text('Local harnesses'),
        children: [
          for (final run in runs)
            SimpleDialogOption(
              onPressed: () => Navigator.of(context).pop(run),
              child: ListTile(
                contentPadding: EdgeInsets.zero,
                title: Text(run.workspace),
                subtitle: Text('${run.harnessId} · ${run.addr}'),
              ),
            ),
        ],
      ),
    );
    if (run == null || !context.mounted) {
      return;
    }
    await _connectToRun(context, registry, run);
  }

  Future<void> _connectToRun(
    BuildContext context,
    HarnessRegistry registry,
    RunInfo run,
  ) async {
    final messenger = ScaffoldMessenger.of(context);
    try {
      final token = await local.readLocalToken(run);
      final name = run.workspace.split('/').lastWhere(
            (part) => part.isNotEmpty,
            orElse: () => run.harnessId,
          );
      final connection = await registry.addLocal(
        name: name,
        url: 'wss://${run.addr}',
        token: token,
        fingerprintSha256: run.certFingerprintSha256,
      );
      if (context.mounted) {
        _openChat(context, connection);
      }
    } catch (error) {
      messenger.showSnackBar(
        SnackBar(content: Text('Could not attach: $error')),
      );
    }
  }

  Future<void> _startLocal(BuildContext context) async {
    final registry = context.read<HarnessRegistry>();
    final messenger = ScaffoldMessenger.of(context);
    LocalHarnessFormState? form = registry.lastLaunchForm;
    final options = await showDialog<LocalHarnessOptions>(
      context: context,
      builder: (_) => StartLocalDialog(
        initialSiloPath: registry.siloPath,
        initialForm: form,
        onFormChanged: (state) => form = state,
      ),
    );
    // The form state is saved on every outcome, so a form dismissed
    // without starting is restored on the next opening.
    final latestForm = form;
    if (latestForm != null) {
      await registry.setLastLaunchForm(latestForm);
    }
    if (options == null || !context.mounted) {
      return;
    }
    await registry.setSiloPath(options.siloBinary);
    if (!context.mounted) {
      return;
    }
    messenger.showSnackBar(
      const SnackBar(content: Text('Starting harness…')),
    );
    final RunInfo? run;
    try {
      run = await local.startLocalHarness(options);
    } on HarnessStartError catch (error) {
      if (context.mounted) {
        await _showStartError(context, options, error);
      }
      return;
    }
    if (!context.mounted) {
      return;
    }
    if (run == null) {
      messenger.showSnackBar(const SnackBar(
        content: Text('The harness did not come up in time.'),
      ));
      return;
    }
    await _connectToRun(context, registry, run);
  }

  Future<void> _showStartError(
    BuildContext context,
    LocalHarnessOptions options,
    HarnessStartError error,
  ) {
    return showDialog<void>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Harness failed to start'),
        content: SizedBox(
          width: 520,
          child: SingleChildScrollView(
            child: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(error.message),
                const SizedBox(height: 12),
                Text('Command', style: Theme.of(context).textTheme.labelSmall),
                SelectableText(
                  runCommandLine(options),
                  style: const TextStyle(fontFamily: monoFontFamily, fontSize: 12),
                ),
                if (error.stderrTail.isNotEmpty) ...[
                  const SizedBox(height: 12),
                  Text('Process output',
                      style: Theme.of(context).textTheme.labelSmall),
                  SelectableText(
                    error.stderrTail,
                    style:
                        const TextStyle(fontFamily: monoFontFamily, fontSize: 12),
                  ),
                ],
              ],
            ),
          ),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(context).pop(),
            child: const Text('Close'),
          ),
        ],
      ),
    );
  }

  void _showAddMenu(BuildContext context) {
    showModalBottomSheet<void>(
      context: context,
      showDragHandle: true,
      builder: (sheetContext) => SafeArea(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            ListTile(
              leading: const Icon(Icons.qr_code_2),
              title: const Text('Pair with a harness'),
              subtitle:
                  const Text('Enter its address and a one-time pairing code'),
              onTap: () {
                Navigator.of(sheetContext).pop();
                _addPaired(context);
              },
            ),
            if (local.localRunsSupported)
              ListTile(
                leading: const Icon(Icons.lan_outlined),
                title: const Text('Connect to a local harness'),
                subtitle: const Text('Pick one already running here'),
                onTap: () {
                  Navigator.of(sheetContext).pop();
                  _attachLocal(context);
                },
              ),
            if (local.canSpawnHarness)
              ListTile(
                leading: const Icon(Icons.rocket_launch_outlined),
                title: const Text('Start a local harness'),
                subtitle: const Text('Choose a workspace directory'),
                onTap: () {
                  Navigator.of(sheetContext).pop();
                  _startLocal(context);
                },
              ),
          ],
        ),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Silo')),
      body: Consumer<HarnessRegistry>(
        builder: (context, registry, _) {
          if (!registry.loaded) {
            return const Center(child: CircularProgressIndicator());
          }
          final endpoints = registry.endpoints;
          if (endpoints.isEmpty) {
            return Center(
              child: Column(
                mainAxisSize: MainAxisSize.min,
                children: [
                  Icon(Icons.hub_outlined,
                      size: 56, color: Theme.of(context).colorScheme.outline),
                  const SizedBox(height: 12),
                  Text('No harnesses yet',
                      style: Theme.of(context).textTheme.titleMedium),
                  const SizedBox(height: 4),
                  Text(
                    'Add one to start a conversation.',
                    style: Theme.of(context).textTheme.bodyMedium?.copyWith(
                          color: Theme.of(context).colorScheme.outline,
                        ),
                  ),
                ],
              ),
            );
          }
          return ListView.builder(
            itemCount: endpoints.length,
            itemBuilder: (context, index) => _HarnessTile(
              endpoint: endpoints[index],
              connection: registry.connectionFor(endpoints[index]),
              onOpen: (connection) => _openChat(context, connection),
              onRemove: () => registry.remove(endpoints[index].id),
            ),
          );
        },
      ),
      floatingActionButton: FloatingActionButton.extended(
        onPressed: () => _showAddMenu(context),
        icon: const Icon(Icons.add),
        label: const Text('Add harness'),
      ),
    );
  }
}

class _HarnessTile extends StatelessWidget {
  const _HarnessTile({
    required this.endpoint,
    required this.connection,
    required this.onOpen,
    required this.onRemove,
  });

  final HarnessEndpoint endpoint;
  final HarnessConnection connection;
  final void Function(HarnessConnection) onOpen;
  final VoidCallback onRemove;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: connection,
      builder: (context, _) {
        final scheme = Theme.of(context).colorScheme;
        final (color, label) = switch (connection.status) {
          ConnectionStatus.connected => (Colors.green, 'Connected'),
          ConnectionStatus.connecting => (Colors.orange, 'Connecting…'),
          ConnectionStatus.authenticating => (
              Colors.orange,
              'Authenticating…'
            ),
          ConnectionStatus.reconnecting => (Colors.orange, 'Reconnecting…'),
          ConnectionStatus.failed => (
              scheme.error,
              connection.lastError ?? 'Authentication failed'
            ),
          ConnectionStatus.disconnected => (scheme.outline, 'Not connected'),
        };
        final unread = connection.unreadCount;
        return ListTile(
          leading: Badge(
            isLabelVisible: unread > 0,
            label: Text('$unread'),
            child: CircleAvatar(
              backgroundColor: scheme.surfaceContainerHighest,
              child: Icon(Icons.hub_outlined, color: color),
            ),
          ),
          title: Text(endpoint.name),
          subtitle: Text(
            '${endpoint.url} · $label',
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
          ),
          trailing: PopupMenuButton<String>(
            onSelected: (value) {
              switch (value) {
                case 'connect':
                  connection.connect();
                case 'disconnect':
                  connection.disconnect();
                case 'remove':
                  onRemove();
              }
            },
            itemBuilder: (context) => [
              if (connection.status == ConnectionStatus.disconnected ||
                  connection.status == ConnectionStatus.failed)
                const PopupMenuItem(
                    value: 'connect', child: Text('Connect'))
              else
                const PopupMenuItem(
                    value: 'disconnect', child: Text('Disconnect')),
              const PopupMenuItem(value: 'remove', child: Text('Remove')),
            ],
          ),
          onTap: () => onOpen(connection),
        );
      },
    );
  }
}

class _PairInput {
  const _PairInput({
    required this.name,
    required this.url,
    required this.code,
    this.fingerprint,
  });

  final String name;
  final String url;
  final String code;
  final String? fingerprint;
}

class _PairDialog extends StatefulWidget {
  const _PairDialog();

  @override
  State<_PairDialog> createState() => _PairDialogState();
}

class _PairDialogState extends State<_PairDialog> {
  final _name = TextEditingController();
  final _url = TextEditingController(text: 'wss://');
  final _code = TextEditingController();
  final _fingerprint = TextEditingController();

  @override
  void dispose() {
    _name.dispose();
    _url.dispose();
    _code.dispose();
    _fingerprint.dispose();
    super.dispose();
  }

  void _submit() {
    final url = _url.text.trim();
    final code = _code.text.trim();
    if (url.isEmpty || code.isEmpty) {
      return;
    }
    final name = _name.text.trim();
    final fingerprint = _fingerprint.text.trim();
    Navigator.of(context).pop(_PairInput(
      name: name.isEmpty ? Uri.tryParse(url)?.host ?? url : name,
      url: url,
      code: code,
      fingerprint: fingerprint.isEmpty ? null : fingerprint,
    ));
  }

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: const Text('Pair with a harness'),
      content: SizedBox(
        width: 380,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            TextField(
              controller: _name,
              decoration: const InputDecoration(
                labelText: 'Name (optional)',
              ),
            ),
            const SizedBox(height: 12),
            TextField(
              controller: _url,
              decoration: const InputDecoration(
                labelText: 'WebSocket URL',
                hintText: 'wss://host:port',
              ),
            ),
            const SizedBox(height: 12),
            TextField(
              controller: _code,
              decoration: const InputDecoration(
                labelText: 'Pairing code',
              ),
              onSubmitted: (_) => _submit(),
            ),
            const SizedBox(height: 12),
            TextField(
              controller: _fingerprint,
              decoration: const InputDecoration(
                labelText: 'Certificate fingerprint (optional)',
                hintText: 'SHA-256 hex, shown next to the pairing code',
              ),
              onSubmitted: (_) => _submit(),
            ),
          ],
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Cancel'),
        ),
        FilledButton(onPressed: _submit, child: const Text('Pair')),
      ],
    );
  }
}

/// The form for starting a new local harness. Pops with the composed
/// [LocalHarnessOptions], or null when cancelled.
class StartLocalDialog extends StatefulWidget {
  const StartLocalDialog({
    super.key,
    this.pickDirectory,
    this.isWorkspaceLocked,
    this.initialSiloPath,
    this.initialForm,
    this.onFormChanged,
    this.resolveSilo,
    this.siloExists,
  });

  /// Directory picker; defaults to the platform file selector.
  final Future<String?> Function()? pickDirectory;

  /// Lock probe used to decide whether `--create` is needed; defaults to
  /// reading the workspace registry.
  final Future<bool> Function(String dir)? isWorkspaceLocked;

  /// Previously saved `silo` binary path, fed to the resolver to prefill
  /// the silo binary field. A non-empty [initialForm] silo path takes
  /// precedence.
  final String? initialSiloPath;

  /// Previously saved form state; when present, every field is prefilled
  /// from it.
  final LocalHarnessFormState? initialForm;

  /// Called with the current form state on every field change. The opener
  /// keeps the latest value and persists it once the dialog closes, so
  /// the state survives a cancel or dismiss as well as a start.
  final ValueChanged<LocalHarnessFormState>? onFormChanged;

  /// Resolves the `silo` binary path from an optional configured path;
  /// defaults to probing `SILO_BIN`, `PATH`, and the conventional install
  /// locations.
  final String? Function(String? configuredPath)? resolveSilo;

  /// File-exists probe for the silo binary field; defaults to the
  /// filesystem.
  final bool Function(String path)? siloExists;

  @override
  State<StartLocalDialog> createState() => _StartLocalDialogState();
}

class _StartLocalDialogState extends State<StartLocalDialog> {
  final _dir = TextEditingController();
  late final TextEditingController _silo;
  final _model =
      TextEditingController(text: LlmBackendChoice.anthropic.defaultModel);
  final _apiKeyEnv =
      TextEditingController(text: LlmBackendChoice.anthropic.defaultApiKeyEnv);
  final _domains = TextEditingController();
  final _allowRead = TextEditingController();
  final _quota = TextEditingController();

  LlmBackendChoice _backend = LlmBackendChoice.anthropic;
  SandboxChoice _sandbox = SandboxChoice.auto;
  bool _create = true;
  String? _quotaError;

  @override
  void initState() {
    super.initState();
    final form = widget.initialForm;
    final resolve = widget.resolveSilo ?? local.resolveSiloBinary;
    final savedSilo = form?.siloPath ?? '';
    final configured =
        savedSilo.isNotEmpty ? savedSilo : widget.initialSiloPath;
    _silo = TextEditingController(text: resolve(configured) ?? savedSilo);
    if (form != null) {
      _dir.text = form.workspaceDir;
      _model.text = form.model;
      _apiKeyEnv.text = form.apiKeyEnv;
      _domains.text = form.domainsText;
      _allowRead.text = form.readAllowlistText;
      _quota.text = form.quotaText;
      _backend = form.backend;
      _sandbox = form.sandbox;
    }
    for (final controller in [
      _dir,
      _silo,
      _model,
      _apiKeyEnv,
      _domains,
      _allowRead,
    ]) {
      controller.addListener(_fieldChanged);
    }
    _dir.addListener(_updateCreate);
    _quota.addListener(_fieldChanged);
    _updateCreate();
  }

  @override
  void dispose() {
    _dir.dispose();
    _silo.dispose();
    _model.dispose();
    _apiKeyEnv.dispose();
    _domains.dispose();
    _allowRead.dispose();
    _quota.dispose();
    super.dispose();
  }

  /// True when the silo binary field holds the path of an existing file.
  bool get _siloFound {
    final path = _silo.text.trim();
    if (path.isEmpty) {
      return false;
    }
    return (widget.siloExists ?? local.siloBinaryExists)(path);
  }

  void _refresh() => setState(() {});

  void _fieldChanged() {
    widget.onFormChanged?.call(_formState);
    _refresh();
  }

  /// The current field values, as handed to [StartLocalDialog.onFormChanged]
  /// for persistence.
  LocalHarnessFormState get _formState => LocalHarnessFormState(
        workspaceDir: _dir.text,
        siloPath: _silo.text,
        backend: _backend,
        model: _model.text,
        apiKeyEnv: _apiKeyEnv.text,
        sandbox: _sandbox,
        domainsText: _domains.text,
        readAllowlistText: _allowRead.text,
        quotaText: _quota.text,
      );

  /// Checks whether the chosen directory is already a locked workspace and
  /// drops `--create` when it is.
  Future<void> _updateCreate() async {
    final dir = _dir.text.trim();
    if (dir.isEmpty) {
      return;
    }
    final probe = widget.isWorkspaceLocked ?? local.isWorkspaceLocked;
    final locked = await probe(dir);
    if (mounted && dir == _dir.text.trim()) {
      setState(() => _create = !locked);
    }
  }

  Future<void> _chooseDirectory() async {
    final pick = widget.pickDirectory ??
        () => getDirectoryPath(confirmButtonText: 'Use as workspace');
    final dir = await pick();
    if (dir != null && mounted) {
      _dir.text = dir;
    }
  }

  /// Switches the backend, replacing the model and API key env var fields
  /// when they still hold the previous backend's defaults.
  void _setBackend(LlmBackendChoice backend) {
    setState(() {
      if (_model.text.trim() == _backend.defaultModel) {
        _model.text = backend.defaultModel;
      }
      if (_apiKeyEnv.text.trim() == _backend.defaultApiKeyEnv) {
        _apiKeyEnv.text = backend.defaultApiKeyEnv;
      }
      _backend = backend;
    });
    widget.onFormChanged?.call(_formState);
  }

  /// The options composed from the current form fields. Null while the
  /// workspace directory is empty, the silo binary field does not point at
  /// an existing file, or the quota does not parse.
  LocalHarnessOptions? get _options {
    final dir = _dir.text.trim();
    if (dir.isEmpty || !_siloFound) {
      return null;
    }
    final quotaText = _quota.text.trim();
    double? quota;
    if (quotaText.isNotEmpty) {
      quota = double.tryParse(quotaText);
      if (quota == null) {
        return null;
      }
    }
    return LocalHarnessOptions(
      workspaceDir: dir,
      siloBinary: _silo.text.trim(),
      createWorkspace: _create,
      backend: _backend,
      model: _model.text.trim(),
      apiKeyEnv: _apiKeyEnv.text.trim(),
      sandbox: _sandbox,
      allowedDomains: splitLines(_domains.text),
      readAllowlist: splitLines(_allowRead.text),
      quotaUsd: quota,
    );
  }

  void _submit() {
    final quotaText = _quota.text.trim();
    if (quotaText.isNotEmpty && double.tryParse(quotaText) == null) {
      setState(() => _quotaError = 'Enter a number, e.g. 2.50');
      return;
    }
    _quotaError = null;
    final options = _options;
    if (options == null) {
      return;
    }
    Navigator.of(context).pop(options);
  }

  @override
  Widget build(BuildContext context) {
    final options = _options;
    return AlertDialog(
      title: const Text('Start a local harness'),
      content: SizedBox(
        width: 480,
        child: SingleChildScrollView(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Row(
                crossAxisAlignment: CrossAxisAlignment.end,
                children: [
                  Expanded(
                    child: TextField(
                      controller: _dir,
                      decoration: const InputDecoration(
                        labelText: 'Workspace directory',
                      ),
                    ),
                  ),
                  const SizedBox(width: 8),
                  TextButton(
                    onPressed: _chooseDirectory,
                    child: const Text('Choose…'),
                  ),
                ],
              ),
              const SizedBox(height: 12),
              TextField(
                controller: _silo,
                decoration: InputDecoration(
                  labelText: 'silo binary',
                  errorText: _siloFound ? null : siloNotFoundMessage,
                  errorMaxLines: 4,
                ),
              ),
              const SizedBox(height: 12),
              DropdownButtonFormField<LlmBackendChoice>(
                initialValue: _backend,
                decoration: const InputDecoration(labelText: 'LLM backend'),
                items: [
                  for (final backend in LlmBackendChoice.values)
                    DropdownMenuItem(
                      value: backend,
                      child: Text(backend.cliName),
                    ),
                ],
                onChanged: (backend) {
                  if (backend != null) {
                    _setBackend(backend);
                  }
                },
              ),
              const SizedBox(height: 12),
              TextField(
                controller: _model,
                decoration: const InputDecoration(labelText: 'Model'),
              ),
              const SizedBox(height: 12),
              TextField(
                controller: _apiKeyEnv,
                decoration: const InputDecoration(
                  labelText: 'API key environment variable',
                ),
              ),
              const SizedBox(height: 12),
              DropdownButtonFormField<SandboxChoice>(
                initialValue: _sandbox,
                decoration: const InputDecoration(labelText: 'Sandbox'),
                items: [
                  for (final sandbox in SandboxChoice.values)
                    DropdownMenuItem(
                      value: sandbox,
                      child: Text(sandbox.cliName),
                    ),
                ],
                onChanged: (sandbox) {
                  if (sandbox != null) {
                    setState(() => _sandbox = sandbox);
                    widget.onFormChanged?.call(_formState);
                  }
                },
              ),
              const SizedBox(height: 12),
              TextField(
                controller: _domains,
                maxLines: 3,
                minLines: 2,
                decoration: const InputDecoration(
                  labelText: 'Allowed domains (one per line)',
                  hintText: 'api.example.com\n*.docs.example.com',
                ),
              ),
              const SizedBox(height: 12),
              TextField(
                controller: _allowRead,
                maxLines: 3,
                minLines: 2,
                decoration: const InputDecoration(
                  labelText: 'Read-allowlist paths (one per line)',
                ),
              ),
              const SizedBox(height: 12),
              TextField(
                controller: _quota,
                decoration: InputDecoration(
                  labelText: 'Dollar quota (optional)',
                  errorText: _quotaError,
                ),
                keyboardType:
                    const TextInputType.numberWithOptions(decimal: true),
              ),
              const SizedBox(height: 16),
              Text('Command', style: Theme.of(context).textTheme.labelSmall),
              const SizedBox(height: 4),
              SelectableText(
                options == null ? '—' : runCommandLine(options),
                style: const TextStyle(fontFamily: monoFontFamily, fontSize: 12),
              ),
            ],
          ),
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Cancel'),
        ),
        FilledButton(
          onPressed: options == null ? null : _submit,
          child: const Text('Start'),
        ),
      ],
    );
  }
}
