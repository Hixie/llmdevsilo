/// Selects the platform backing for the non-secret preferences document:
/// a JSON file in the app support directory where there is a filesystem,
/// browser storage on the web.
library;

export 'preferences_web.dart'
    if (dart.library.io) 'preferences_io.dart';
