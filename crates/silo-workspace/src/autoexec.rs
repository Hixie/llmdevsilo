//! Matcher for auto-exec surfaces: files whose contents the host may
//! execute outside the sandbox (git hooks and configuration, direnv files,
//! IDE configuration, package manager scripts, build scripts).
//!
//! The unlock report flags every changed path that matches a rule, so the
//! user reviews those first. The rules are table-driven: adding a surface
//! is one line in [`RULES`].

/// A pattern over workspace-relative paths with `/` separators.
enum Pattern {
    /// Matches paths strictly inside a directory chain with these names,
    /// at any depth.
    UnderDir(&'static [&'static str]),
    /// Matches a file or directory with this exact name, at any depth.
    Name(&'static str),
    /// Matches paths whose final segments equal this chain, at any depth.
    PathSuffix(&'static [&'static str]),
    /// Matches names ending with this suffix, at any depth.
    NameSuffix(&'static str),
}

impl Pattern {
    fn matches(&self, segments: &[&str]) -> bool {
        match self {
            Pattern::UnderDir(dirs) => segments[..segments.len() - 1]
                .windows(dirs.len())
                .any(|window| window == *dirs),
            Pattern::Name(name) => segments.last() == Some(name),
            Pattern::PathSuffix(suffix) => segments.ends_with(suffix),
            Pattern::NameSuffix(suffix) => {
                segments.last().is_some_and(|name| name.ends_with(suffix))
            }
        }
    }
}

const RULES: &[(Pattern, &str)] = &[
    (
        Pattern::UnderDir(&[".git", "hooks"]),
        "git hook: git runs these scripts on commit, checkout, merge, push, and other operations",
    ),
    (
        Pattern::PathSuffix(&[".git", "config"]),
        "git repository configuration: core.hooksPath, core.fsmonitor, and filter drivers name programs git executes",
    ),
    (
        Pattern::Name(".gitattributes"),
        "git attributes: filter and diff driver assignments make git run the bound driver commands on checkout, diff, and other operations",
    ),
    (
        Pattern::Name(".gitmodules"),
        "git submodules: URL and path changes control what git fetches and checks out on submodule update",
    ),
    (
        Pattern::Name(".envrc"),
        "direnv configuration: the shell executes .envrc when entering the directory",
    ),
    (
        Pattern::UnderDir(&[".direnv"]),
        "direnv state: contents are loaded by direnv when entering the directory",
    ),
    (
        Pattern::UnderDir(&[".vscode"]),
        "VS Code workspace configuration: tasks.json, settings.json, and launch.json can run commands when the folder is opened",
    ),
    (
        Pattern::UnderDir(&[".idea"]),
        "JetBrains IDE configuration: run configurations and tool settings can run commands when the project is opened",
    ),
    (
        Pattern::Name("package.json"),
        "npm manifest: lifecycle scripts such as postinstall run on package installation",
    ),
    (
        Pattern::UnderDir(&[".husky"]),
        "husky git hook: git runs these scripts on commit and other operations",
    ),
    (
        Pattern::Name(".pre-commit-config.yaml"),
        "pre-commit configuration: defines hooks that run on git commit",
    ),
    (
        Pattern::Name("build.rs"),
        "cargo build script: compiled and run by cargo build",
    ),
    (
        Pattern::PathSuffix(&[".cargo", "config.toml"]),
        "cargo configuration: runner and rustc-wrapper settings redirect build and run commands",
    ),
    (
        Pattern::PathSuffix(&[".cargo", "config"]),
        "cargo configuration: runner and rustc-wrapper settings redirect build and run commands",
    ),
    (
        Pattern::NameSuffix(".code-workspace"),
        "VS Code workspace file: embedded settings and tasks can run commands when opened",
    ),
];

/// Returns the reason string when the workspace-relative path is a known
/// auto-exec surface. The first matching rule wins.
pub fn match_path(rel_path: &str) -> Option<&'static str> {
    let segments: Vec<&str> = rel_path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return None;
    }
    RULES
        .iter()
        .find(|(pattern, _)| pattern.matches(&segments))
        .map(|(_, reason)| *reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_flagged(path: &str, reason_contains: &str) {
        let reason = match_path(path)
            .unwrap_or_else(|| panic!("expected {path} to be flagged as auto-exec"));
        assert!(
            reason.contains(reason_contains),
            "reason for {path} was {reason:?}, expected it to mention {reason_contains:?}"
        );
    }

    #[test]
    fn git_hooks_match() {
        assert_flagged(".git/hooks/pre-commit", "git hook");
        assert_flagged("vendor/dep/.git/hooks/post-checkout", "git hook");
        assert!(match_path(".git/hooks").is_none());
        assert!(match_path(".git/description").is_none());
    }

    #[test]
    fn git_config_matches() {
        assert_flagged(".git/config", "git repository configuration");
        assert_flagged("vendor/dep/.git/config", "git repository configuration");
        assert!(match_path("git/config").is_none());
        assert!(match_path("config").is_none());
        assert!(match_path(".git/config.worktree").is_none());
    }

    #[test]
    fn gitattributes_matches() {
        assert_flagged(".gitattributes", "filter");
        assert_flagged("sub/dir/.gitattributes", "filter");
        assert!(match_path("gitattributes").is_none());
    }

    #[test]
    fn gitmodules_matches() {
        assert_flagged(".gitmodules", "submodule");
        assert_flagged("sub/.gitmodules", "submodule");
        assert!(match_path("gitmodules").is_none());
    }

    #[test]
    fn envrc_matches() {
        assert_flagged(".envrc", "direnv");
        assert_flagged("sub/dir/.envrc", "direnv");
        assert!(match_path("envrc").is_none());
    }

    #[test]
    fn direnv_dir_matches() {
        assert_flagged(".direnv/bin/tool", "direnv");
        assert!(match_path("direnv/bin/tool").is_none());
    }

    #[test]
    fn vscode_dir_matches() {
        assert_flagged(".vscode/tasks.json", "VS Code");
        assert_flagged(".vscode/settings.json", "VS Code");
        assert_flagged(".vscode/launch.json", "VS Code");
        assert_flagged("sub/.vscode/extensions.json", "VS Code");
        assert!(match_path("vscode/tasks.json").is_none());
    }

    #[test]
    fn idea_dir_matches() {
        assert_flagged(".idea/workspace.xml", "JetBrains");
        assert!(match_path("idea/workspace.xml").is_none());
    }

    #[test]
    fn package_json_matches_at_any_depth() {
        assert_flagged("package.json", "lifecycle");
        assert_flagged("packages/app/package.json", "lifecycle");
        assert!(match_path("package.json5").is_none());
        assert!(match_path("not-package.json").is_none());
    }

    #[test]
    fn husky_dir_matches() {
        assert_flagged(".husky/pre-commit", "husky");
        assert!(match_path("husky/pre-commit").is_none());
    }

    #[test]
    fn pre_commit_config_matches() {
        assert_flagged(".pre-commit-config.yaml", "pre-commit");
        assert!(match_path(".pre-commit-config.yml").is_none());
    }

    #[test]
    fn build_rs_matches_at_any_depth() {
        assert_flagged("build.rs", "build script");
        assert_flagged("crates/foo/build.rs", "build script");
        assert!(match_path("src/builder.rs").is_none());
    }

    #[test]
    fn cargo_config_matches() {
        assert_flagged(".cargo/config.toml", "cargo configuration");
        assert_flagged(".cargo/config", "cargo configuration");
        assert_flagged("sub/.cargo/config.toml", "cargo configuration");
        assert!(match_path(".cargo/audit.toml").is_none());
        assert!(match_path("x.cargo/config").is_none());
    }

    #[test]
    fn code_workspace_extension_matches() {
        assert_flagged("project.code-workspace", "VS Code workspace file");
        assert_flagged("sub/dir/my.code-workspace", "VS Code workspace file");
        assert!(match_path("code-workspace").is_none());
    }

    #[test]
    fn ordinary_paths_do_not_match() {
        assert!(match_path("src/main.rs").is_none());
        assert!(match_path("README.md").is_none());
        assert!(match_path("docs/notes.txt").is_none());
        assert!(match_path("").is_none());
    }
}
