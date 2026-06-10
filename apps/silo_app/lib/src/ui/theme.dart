/// App-wide theme: teal Material 3 color scheme with bundled Inter and
/// JetBrains Mono fonts.
library;

import 'package:flutter/material.dart';

/// Font family for monospace content: tool payloads, command previews, and
/// sandbox path lists. Bundled in assets/fonts/.
const String monoFontFamily = 'JetBrains Mono';

/// Seed color shared with the launcher icon background.
const Color siloSeedColor = Color(0xFF356859);

/// Shared left edge for transcript content (prompts, assistant text, tool
/// tiles) and the input field below it.
const double contentGutter = 16;

/// Builds the app theme for [brightness]. Inter is the default family; the
/// text theme uses medium-to-semibold titles, a slightly smaller body size,
/// and near-zero letter spacing.
ThemeData siloTheme(Brightness brightness) {
  final base = ThemeData(
    colorScheme: ColorScheme.fromSeed(
      seedColor: siloSeedColor,
      brightness: brightness,
    ),
    fontFamily: 'Inter',
    visualDensity: VisualDensity.adaptivePlatformDensity,
  );
  final text = base.textTheme;
  return base.copyWith(
    textTheme: text.copyWith(
      titleLarge: text.titleLarge
          ?.copyWith(fontWeight: FontWeight.w600, letterSpacing: -0.2),
      titleMedium: text.titleMedium
          ?.copyWith(fontWeight: FontWeight.w600, letterSpacing: -0.1),
      titleSmall: text.titleSmall?.copyWith(fontWeight: FontWeight.w600),
      bodyLarge: text.bodyLarge?.copyWith(fontSize: 15, letterSpacing: 0),
      bodyMedium: text.bodyMedium?.copyWith(fontSize: 13.5, letterSpacing: 0),
      bodySmall: text.bodySmall?.copyWith(letterSpacing: 0.1),
      labelLarge: text.labelLarge
          ?.copyWith(fontWeight: FontWeight.w500, letterSpacing: 0.1),
    ),
  );
}
