import 'package:flutter/material.dart';
import 'package:provider/provider.dart';

import 'src/connection/harness_registry.dart';
import 'src/connection/secret_store.dart';
import 'src/ui/home_screen.dart';

void main() {
  WidgetsFlutterBinding.ensureInitialized();
  runApp(const SiloApp());
}

class SiloApp extends StatelessWidget {
  const SiloApp({super.key});

  @override
  Widget build(BuildContext context) {
    return ChangeNotifierProvider<HarnessRegistry>(
      create: (_) => HarnessRegistry(secrets: SecureSecretStore())..load(),
      child: MaterialApp(
        title: 'Silo',
        debugShowCheckedModeBanner: false,
        theme: ThemeData(
          colorScheme: ColorScheme.fromSeed(
            seedColor: const Color(0xFF356859),
          ),
          visualDensity: VisualDensity.adaptivePlatformDensity,
        ),
        darkTheme: ThemeData(
          colorScheme: ColorScheme.fromSeed(
            seedColor: const Color(0xFF356859),
            brightness: Brightness.dark,
          ),
          visualDensity: VisualDensity.adaptivePlatformDensity,
        ),
        home: const HomeScreen(),
      ),
    );
  }
}
