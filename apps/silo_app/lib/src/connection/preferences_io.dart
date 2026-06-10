/// Preferences document for platforms with a filesystem: a plain JSON
/// file in the application support directory. Preferences hold no
/// secrets, so they stay out of the platform keystore and never trigger
/// keychain prompts.
library;

import 'dart:io';

import 'package:path_provider/path_provider.dart';

import 'secret_store.dart';

/// A document in a file, created (with its parent directory) on first
/// write.
class FileDocumentStore implements DocumentStore {
  FileDocumentStore(this._file);

  final Future<File> Function() _file;

  @override
  Future<String?> read() async {
    final file = await _file();
    if (!await file.exists()) {
      return null;
    }
    return file.readAsString();
  }

  @override
  Future<void> write(String contents) async {
    final file = await _file();
    await file.parent.create(recursive: true);
    await file.writeAsString(contents);
  }
}

/// The app's preferences document: `preferences.json` in the application
/// support directory.
DocumentStore createPreferencesDocument() {
  return FileDocumentStore(() async {
    final dir = await getApplicationSupportDirectory();
    return File('${dir.path}/preferences.json');
  });
}
