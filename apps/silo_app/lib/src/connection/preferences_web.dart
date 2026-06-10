/// Preferences document for the web, where there is no filesystem: one
/// browser-storage entry. Browser storage raises no permission prompts,
/// so consolidation matters only for symmetry with the io layout.
library;

import 'secret_store.dart';

DocumentStore createPreferencesDocument() =>
    SecureDocumentStore('silo/preferences');
