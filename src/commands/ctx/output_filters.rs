//! The bundled default `[[output.filter]]` rule set (see `config::
//! OutputConfig::filter_defaults`'s own doc comment) that ships zirv's own
//! out-of-the-box compaction for common noisy build/install/download/test
//! tools, so `Generic`-scope compaction is useful with zero operator
//! configuration. `CtxConfig::load` appends every one of these whose `name`
//! the operator did not already declare in their own `[[output.filter]]`
//! list, right after `validate_output_filter_rules` has checked the
//! operator's own rules -- see that call site's own comment for the merge
//! order.

use super::config::OutputFilterRule;

/// A line made ONLY of progress-bar glyphs, whitespace, numbers, percentages,
/// byte-rate tokens (`1.2M/s`, `62 kB`, `4.0 MiB`) and `eta` markers, which
/// must contain at least one digit or a run of three bar glyphs -- shared
/// by every bundled rule below and by the `progress-noise` catch-all as the
/// one pattern that strips a spinner/progress-bar line without ever
/// touching a line holding an ordinary word. Letters are only allowed
/// inside those size/rate/eta tokens, so `steps:`, `set -e` or `1 test`
/// never match; a bare timestamp or version number does, which a summary of
/// an already-oversized result can afford to lose. `progress_token!` is one
/// alternative of it: a bar/punctuation glyph, a size or rate (`62 kB`,
/// `1.2M/s`, `4.0 MiB`), an `eta` marker, or a digit.
macro_rules! progress_token {
    () => {
        concat!(
            r"[\s%=#/.,:\-\[\]()>|\\*░█▓▒⣾⣽⣻⢿⡿⣟⣯⣷]",
            r"|\d+(?:\.\d+)?\s*(?:[KMGTkmgt]i?[Bb]?(?:/s|ps)?|ms|[smhd])\b",
            r"|\b(?:eta|ETA)\b",
            r"|\d"
        )
    };
}
const PROGRESS_LINE: &str = concat!(
    r"^(?:",
    progress_token!(),
    r")*(?:\d|[=#░█▓▒]{3})(?:",
    progress_token!(),
    r")*$"
);

fn rule(name: &str, match_command: &str, strip_lines: &[&str]) -> OutputFilterRule {
    OutputFilterRule {
        name: name.to_string(),
        match_command: match_command.to_string(),
        strip_lines: strip_lines.iter().map(|p| p.to_string()).collect(),
        keep_lines: Vec::new(),
        truncate_line_at: None,
        max_lines: None,
        match_output: None,
    }
}

/// The bundled default rules, in fixed declaration order -- first match
/// wins (`find_matching_output_filter`), so every specific rule comes
/// before the `progress-noise` catch-all.
pub(crate) fn bundled_output_filter_rules() -> Vec<OutputFilterRule> {
    vec![
        rule(
            "pkg-python",
            r"^(sudo )?(pip[0-9.]*|pipx|uv|poetry|pdm|conda|mamba)\b",
            &[
                r"^\s*(Collecting |Downloading |Using cached |Requirement already satisfied|Preparing metadata|Building wheels? for|Created wheel for|Stored in directory|Obtaining |Resolving deltas)",
                PROGRESS_LINE,
            ],
        ),
        rule(
            "pkg-system",
            r"^(sudo )?(apt|apt-get|dpkg|dnf|yum|apk|pacman|zypper|brew|choco|winget|scoop)\b",
            &[
                r"^(Get|Hit|Ign):\d+ ",
                r"^(Reading package lists|Building dependency tree|Reading state information)",
                r"^\(Reading database",
                r"^(Selecting previously unselected package|Preparing to unpack|Unpacking |Setting up |Processing triggers for )",
                r"^==> (Downloading|Fetching|Pouring)",
                r"^Already downloaded: ",
                r"^#{5,}",
                PROGRESS_LINE,
            ],
        ),
        rule(
            "download-progress",
            r"^(curl|wget|Invoke-WebRequest|iwr|aria2c)\b",
            &[
                r"^\s*% Total\s+% Received",
                r"^\s*Dload\s+Upload",
                r"^\s*\d+\s+\d+[kMG]?\s+\d+\s+\d+[kMG]?\s+\d+\s+\d+",
                r"^\s*\d+K \.+",
                PROGRESS_LINE,
            ],
        ),
        rule(
            "build-steps",
            r"^(cmake|ninja|meson|bazel|bazelisk|buck2?)\b",
            &[
                r"^\[\s*\d+/\d+\]\s+(Building|Linking|Generating|Compiling|Creating|Copying|Running|Scanning)",
                r"^\[\s*\d+%\]\s+(Building|Linking|Scanning|Generating|Built target)",
                r"^\[\d+ / \d+\] ",
                PROGRESS_LINE,
            ],
        ),
        rule(
            "toolchain-install",
            r"^(rustup|nvm|fnm|volta|pyenv|rbenv|asdf|mise)\b",
            &[
                r"^info: (downloading|installing|checking|syncing|removing|latest update|profile set)",
                r"^\s*[0-9.]+ [KMG]iB / [0-9.]+ [KMG]iB",
                PROGRESS_LINE,
            ],
        ),
        rule(
            "infra-refresh",
            r"^(terraform|tofu|terragrunt|pulumi|cdk|cdktf)\b",
            &[
                r"^\S+: (Refreshing state\.\.\.|Reading\.\.\.|Read complete after|Still (creating|destroying|modifying|reading)\.\.\.)",
                PROGRESS_LINE,
            ],
        ),
        rule(
            "test-runner-js",
            r"^(jest|vitest|mocha|ava|tap|karma|playwright|cypress|deno test|bun test)\b",
            &[
                r"^\s*[✓✔√] ",
                r"^\s*PASS\b",
                r"^\s*ok \d+ - ",
                PROGRESS_LINE,
            ],
        ),
        rule("progress-noise", r"^", &[PROGRESS_LINE]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::config::validate_output_filter_rules;
    use crate::commands::ctx::output::apply_operator_output_filter;

    #[test]
    fn bundled_rules_pass_validation() {
        let rules = bundled_output_filter_rules();
        validate_output_filter_rules(&rules).expect("bundled rules must be valid");
        let mut names = std::collections::HashSet::new();
        for rule in &rules {
            assert!(
                names.insert(rule.name.as_str()),
                "duplicate name {}",
                rule.name
            );
        }
    }

    #[test]
    fn progress_line_matches_positive_samples() {
        let re = regex::Regex::new(PROGRESS_LINE).expect("compiles");
        for sample in [
            "[=====>     ] 45%",
            "████████░░ 80%",
            "######## 12.3%",
            "  50% [=====>   ] 1.2M/s eta 3s",
            "  62 kB / 4.0 MiB (1%)",
            "⣾ 12/300",
            "==========>",
        ] {
            assert!(re.is_match(sample), "expected match: {sample:?}");
        }
    }

    #[test]
    fn progress_line_rejects_negative_samples() {
        let re = regex::Regex::new(PROGRESS_LINE).expect("compiles");
        for sample in [
            "error: foo",
            "======= 3 failed =======",
            "--- a/file",
            "-- separator",
            "steps:",
            "set -e",
            "1 test",
            "meta: base",
            "Compiling foo v0.1.0",
        ] {
            assert!(!re.is_match(sample), "expected no match: {sample:?}");
        }
    }

    fn only(name: &str) -> Vec<OutputFilterRule> {
        bundled_output_filter_rules()
            .into_iter()
            .filter(|r| r.name == name)
            .collect()
    }

    #[test]
    fn pkg_python_strips_noise_keeps_signal() {
        let rules = only("pkg-python");
        let raw = "Collecting requests\n\
                   Downloading requests-2.31.0-py3-none-any.whl (62 kB)\n\
                   [=====>     ] 45%\n\
                   Installing collected packages: requests\n\
                   ERROR: Could not find a version that satisfies the requirement badpkg\n\
                   Successfully installed requests-2.31.0\n";
        let out = apply_operator_output_filter(&rules, "pip install requests", raw, "hint")
            .expect("rule must match");
        assert!(!out.contains("Collecting"));
        assert!(!out.contains("Downloading"));
        assert!(!out.contains("45%"));
        assert!(out.contains("ERROR: Could not find"));
        assert!(out.contains("Successfully installed requests-2.31.0"));
    }

    #[test]
    fn pkg_system_strips_noise_keeps_signal() {
        let rules = only("pkg-system");
        let raw = "Reading package lists... Done\n\
                   Building dependency tree... Done\n\
                   (Reading database ... 5%\n\
                   Selecting previously unselected package foo.\n\
                   Setting up foo (1.0) ...\n\
                   E: Unable to locate package badpkg\n\
                   foo is now installed.\n";
        let out = apply_operator_output_filter(&rules, "apt-get install foo", raw, "hint")
            .expect("rule must match");
        assert!(!out.contains("Reading package lists"));
        assert!(!out.contains("Building dependency tree"));
        assert!(!out.contains("Selecting previously unselected"));
        assert!(!out.contains("Setting up"));
        assert!(out.contains("E: Unable to locate package badpkg"));
        assert!(out.contains("foo is now installed."));
    }

    #[test]
    fn download_progress_strips_noise_keeps_signal() {
        let rules = only("download-progress");
        let raw = "  % Total    % Received % Xferd  Average Speed   Time\n\
                   \x20 Dload  Upload   Total   Spent    Left  Speed\n\
                   100  1024k  100  1024k    0     0  5000k      0 --:--:-- --:--:-- --:--:-- 5000k\n\
                   curl: (22) The requested URL returned error: 404\n\
                   saved to 'file.tar.gz'\n";
        let out = apply_operator_output_filter(
            &rules,
            "curl -O https://example.com/file.tar.gz",
            raw,
            "hint",
        )
        .expect("rule must match");
        assert!(!out.contains("% Total"));
        assert!(!out.contains("Dload"));
        assert!(!out.contains("5000k"));
        assert!(out.contains("curl: (22)"));
        assert!(out.contains("saved to 'file.tar.gz'"));
    }

    #[test]
    fn build_steps_strips_noise_keeps_signal() {
        let rules = only("build-steps");
        let raw = "[1/50] Building CXX object foo.cpp.o\n\
                   [2/50] Linking CXX executable foo\n\
                   [ 10%] Building CXX object bar.cpp.o\n\
                   FAILED: bar.cpp.o\n\
                   error: undefined reference to `missing_symbol`\n\
                   [50/50] Built target foo\n";
        let out = apply_operator_output_filter(&rules, "ninja -C build", raw, "hint")
            .expect("rule must match");
        assert!(!out.contains("[1/50] Building"));
        assert!(!out.contains("[2/50] Linking"));
        assert!(!out.contains("[ 10%] Building"));
        assert!(out.contains("FAILED: bar.cpp.o"));
        assert!(out.contains("undefined reference"));
    }

    #[test]
    fn toolchain_install_strips_noise_keeps_signal() {
        let rules = only("toolchain-install");
        let raw = "info: downloading component 'rust-std'\n\
                   info: installing component 'rust-std'\n\
                   50.3 MiB / 50.3 MiB\n\
                   error: could not rename downloaded file\n\
                   info: default toolchain set to 'stable'\n";
        let out = apply_operator_output_filter(&rules, "rustup update", raw, "hint")
            .expect("rule must match");
        assert!(!out.contains("downloading component"));
        assert!(!out.contains("installing component"));
        assert!(!out.contains("MiB /"));
        assert!(out.contains("error: could not rename"));
        assert!(out.contains("default toolchain set to 'stable'"));
    }

    #[test]
    fn infra_refresh_strips_noise_keeps_signal() {
        let rules = only("infra-refresh");
        let raw = "aws_instance.web: Refreshing state... [id=i-0123]\n\
                   aws_instance.web: Still creating... [10s elapsed]\n\
                   aws_instance.web: Read complete after 2s\n\
                   Error: creating EC2 Instance: InvalidParameterValue\n\
                   Apply complete! Resources: 1 added, 0 changed, 0 destroyed.\n";
        let out = apply_operator_output_filter(&rules, "terraform apply", raw, "hint")
            .expect("rule must match");
        assert!(!out.contains("Refreshing state"));
        assert!(!out.contains("Still creating"));
        assert!(!out.contains("Read complete after"));
        assert!(out.contains("Error: creating EC2 Instance"));
        assert!(out.contains("Apply complete!"));
    }

    #[test]
    fn test_runner_js_strips_noise_keeps_signal() {
        let rules = only("test-runner-js");
        let raw = "✓ adds numbers\n\
                   PASS src/math.test.js\n\
                   ok 1 - basic assertion\n\
                   ✗ subtracts numbers\n\
                   FAIL src/math.test.js\n\
                   Tests: 1 failed, 1 passed, 2 total\n";
        let out =
            apply_operator_output_filter(&rules, "jest", raw, "hint").expect("rule must match");
        assert!(!out.contains("✓ adds numbers"));
        assert!(!out.contains("PASS src/math.test.js"));
        assert!(!out.contains("ok 1 - basic assertion"));
        assert!(out.contains("FAIL src/math.test.js"));
        assert!(out.contains("Tests: 1 failed, 1 passed, 2 total"));
    }

    #[test]
    fn progress_noise_catch_all_strips_only_progress_lines() {
        let rules = only("progress-noise");
        let raw = "some ordinary informative line\n\
                   [=====>     ] 45%\n\
                   error: something went wrong\n";
        let out = apply_operator_output_filter(&rules, "my-custom-tool run", raw, "hint")
            .expect("rule must match");
        assert!(!out.contains('%'));
        assert!(out.contains("some ordinary informative line"));
        assert!(out.contains("error: something went wrong"));
    }
}
