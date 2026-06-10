import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import 'src/connection/harness_registry.dart';
import 'src/connection/preferences.dart';
import 'src/connection/secret_store.dart';
import 'src/ui/home_screen.dart';
import 'src/ui/theme.dart';

void main() {
  WidgetsFlutterBinding.ensureInitialized();
  runApp(const SiloApp());
}

class SiloApp extends StatefulWidget {
  const SiloApp({super.key});

  @override
  State<SiloApp> createState() => _SiloAppState();
}

class _SiloAppState extends State<SiloApp> {
  late final HarnessRegistry _registry;

  @override
  void initState() {
    super.initState();
    _registry = HarnessRegistry(
      // Real secrets: one keystore item holding one JSON document, read
      // lazily and at most once per run.
      secrets: JsonDocumentStore(SecureDocumentStore('silo/secrets')),
      // Everything non-secret: a plain JSON preferences file.
      settings: JsonDocumentStore(createPreferencesDocument()),
      // The pre-consolidation layout, read once to migrate.
      legacySecrets: SecureSecretStore(),
    );
    // The home screen renders in its loading state first; storage (and
    // with it any keychain prompt) is touched only after the first frame.
    WidgetsBinding.instance.addPostFrameCallback((_) {
      _registry.load();
    });
  }

  @override
  void dispose() {
    _registry.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return ChangeNotifierProvider<HarnessRegistry>.value(
      value: _registry,
      child: MaterialApp(
        title: 'Silo',
        debugShowCheckedModeBanner: false,
        theme: siloTheme(Brightness.light),
        darkTheme: siloTheme(Brightness.dark),
        home: const HomeScreen(),
      ),
    );
  }
}
