//! Golden-file tests for every wire target. Every test runs in a scratch dir:
//! HOME, the state dir, the pi and Eukhe agent dirs and T3CODE_HOME all point
//! into it, and keys go to an in-memory store, never the OS keychain.
//!
//! `UPDATE_GOLDEN=1 cargo test -p fonsviva-wire` rewrites the expected files.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fonsviva_wire::{
    Endpoint, MemoryKeys, Model, Os, RevertOutcome, State, Target, Wire, WireEnv, WireError,
    read_command,
};
use secrecy::SecretString;
use sha2::{Digest, Sha256};

struct Scratch {
    _dir: tempfile::TempDir,
    root: PathBuf,
    wire: Wire,
    keys: Arc<MemoryKeys>,
}

impl Scratch {
    fn new() -> Self {
        Self::with_pi(true)
    }

    fn with_pi(pi_on_path: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        if pi_on_path {
            let name = if cfg!(windows) { "pi.cmd" } else { "pi" };
            fs::write(bin.join(name), "#!/bin/sh\n").unwrap();
        }
        let mut vars = BTreeMap::new();
        vars.insert(
            "HOME".to_string(),
            root.join("home").to_string_lossy().into_owned(),
        );
        vars.insert(
            "USERPROFILE".to_string(),
            root.join("home").to_string_lossy().into_owned(),
        );
        vars.insert(
            "PI_CODING_AGENT_DIR".to_string(),
            root.join("pi-agent").to_string_lossy().into_owned(),
        );
        vars.insert(
            "EUKHE_CODING_AGENT_DIR".to_string(),
            root.join("eukhe-agent").to_string_lossy().into_owned(),
        );
        vars.insert(
            "T3CODE_HOME".to_string(),
            root.join("t3").to_string_lossy().into_owned(),
        );
        vars.insert("PATH".to_string(), bin.to_string_lossy().into_owned());
        let env = WireEnv {
            os: Os::current(),
            vars,
            shared_dir: root.join("fonsviva-shared"),
        };
        let keys = Arc::new(MemoryKeys::default());
        let wire = Wire::new(env, keys.clone());
        Self {
            _dir: dir,
            root,
            wire,
            keys,
        }
    }

    fn path(&self, target: Target) -> PathBuf {
        self.wire.env().target_path(target).unwrap().unwrap()
    }

    fn seed(&self, target: Target, text: &str) {
        let path = self.path(target);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
    }

    fn read(&self, target: Target) -> String {
        fs::read_to_string(self.path(target)).unwrap()
    }

    fn normalize(&self, text: &str) -> String {
        let pi = self
            .root
            .join("bin")
            .join(if cfg!(windows) { "pi.cmd" } else { "pi" });
        let pi = serde_json::to_string(&pi.to_string_lossy()).unwrap();
        text.replace(&pi[1..pi.len() - 1], "<PI>")
    }

    fn backups(&self, target: Target) -> usize {
        let dir = self
            .root
            .join("fonsviva-shared")
            .join("backups")
            .join("wire")
            .join(target.as_str());
        fs::read_dir(dir)
            .map(|entries| entries.count())
            .unwrap_or(0)
    }
}

fn endpoint() -> Endpoint {
    Endpoint {
        base_url: "http://127.0.0.1:7341/v1".to_string(),
        api_key: None,
        models: vec![
            Model {
                id: "qwen3-4b-q4".into(),
                name: "Qwen3 4B".into(),
                context_window: 32768,
                max_tokens: 8192,
                reasoning: true,
            },
            Model {
                id: "lfm3-1.7b".into(),
                name: String::new(),
                context_window: 32768,
                max_tokens: 4096,
                reasoning: false,
            },
        ],
    }
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
}

fn fixture(target: Target, name: &str) -> String {
    fs::read_to_string(golden_dir().join(target.as_str()).join(name)).unwrap()
}

fn check_golden(target: Target, name: &str, actual: &str) {
    let path = golden_dir().join(target.as_str()).join(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::write(&path, actual).unwrap();
        return;
    }
    let expected = fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing golden file {}", path.display()));
    assert_eq!(actual, expected, "golden mismatch for {}", path.display());
}

fn sha(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn file_targets() -> [Target; 3] {
    [Target::Pi, Target::Eukhe, Target::T3code]
}

#[test]
fn fresh_apply_then_revert_removes_the_file() {
    for target in file_targets() {
        let scratch = Scratch::new();
        assert_eq!(scratch.wire.status(target).unwrap().state, State::Absent);
        let report = scratch.wire.apply(target, &endpoint(), false).unwrap();
        assert!(report.changed);
        assert!(report.backup.is_none(), "{target}: nothing to back up");
        check_golden(
            target,
            "fresh.applied.json",
            &scratch.normalize(&scratch.read(target)),
        );
        assert_eq!(scratch.wire.status(target).unwrap().state, State::Applied);
        let revert = scratch.wire.revert(target, false).unwrap();
        assert_eq!(revert.outcome, RevertOutcome::Restored);
        assert!(
            !scratch.path(target).exists(),
            "{target}: a file Fonsviva created is removed on revert"
        );
        assert_eq!(scratch.wire.status(target).unwrap().state, State::Absent);
    }
}

#[test]
fn existing_file_keeps_other_keys_and_reverts_byte_identical() {
    for target in file_targets() {
        let scratch = Scratch::new();
        let before = fixture(target, "existing.before.json");
        scratch.seed(target, &before);
        let report = scratch.wire.apply(target, &endpoint(), false).unwrap();
        let backup = report.backup.clone().expect("existing file is backed up");
        assert_eq!(fs::read_to_string(&backup).unwrap(), before);
        let applied = scratch.read(target);
        check_golden(
            target,
            "existing.applied.json",
            &scratch.normalize(&applied),
        );
        let other_lines: Vec<&str> = before.lines().collect();
        for line in &other_lines {
            let line = line.trim_end_matches(',');
            assert!(applied.contains(line), "{target}: lost line {line:?}");
        }
        assert!(!report.diff.is_empty());
        let revert = scratch.wire.revert(target, false).unwrap();
        assert_eq!(revert.outcome, RevertOutcome::Restored);
        assert_eq!(
            sha(&scratch.read(target)),
            sha(&before),
            "{target}: revert is byte-identical"
        );
    }
}

#[test]
fn double_apply_is_idempotent_and_keeps_the_first_backup() {
    for target in file_targets() {
        let scratch = Scratch::new();
        scratch.seed(target, &fixture(target, "existing.before.json"));
        let first = scratch.wire.apply(target, &endpoint(), false).unwrap();
        let after_first = scratch.read(target);
        let second = scratch.wire.apply(target, &endpoint(), false).unwrap();
        assert!(!second.changed, "{target}: second apply changes nothing");
        assert!(second.diff.is_empty());
        assert_eq!(scratch.read(target), after_first);
        assert_eq!(
            second.backup, first.backup,
            "{target}: the original backup is kept"
        );
        assert_eq!(scratch.backups(target), 1);
    }
}

#[test]
fn drifted_entry_is_reported_and_revert_keeps_other_edits() {
    for target in [Target::Pi, Target::Eukhe] {
        let scratch = Scratch::new();
        scratch.seed(target, &fixture(target, "existing.before.json"));
        scratch.wire.apply(target, &endpoint(), false).unwrap();
        let edited = scratch
            .read(target)
            .replace("http://127.0.0.1:7341/v1", "http://127.0.0.1:9999/v1");
        fs::write(scratch.path(target), &edited).unwrap();
        assert_eq!(scratch.wire.status(target).unwrap().state, State::Drifted);
        let refused = scratch.wire.revert(target, false).unwrap_err();
        assert_eq!(refused.code(), "drifted");
        let WireError::Drifted { diff, .. } = &refused else {
            panic!("{target}: expected drifted, got {refused:?}")
        };
        assert!(
            diff.contains("-") && diff.contains("+") && diff.contains("9999"),
            "{target}: diff shows the change: {diff}"
        );
        assert_eq!(
            scratch.read(target),
            edited,
            "{target}: a refused revert writes nothing"
        );
        let refused_apply = scratch.wire.apply(target, &endpoint(), false).unwrap_err();
        assert_eq!(refused_apply.code(), "drifted");
        assert_eq!(
            scratch.read(target),
            edited,
            "{target}: a refused apply writes nothing"
        );

        let revert = scratch.wire.revert(target, true).unwrap();
        assert_eq!(revert.outcome, RevertOutcome::RemovedEntries);
        let second = revert
            .drift_backup
            .expect("a confirmed revert saves the drifted file first");
        assert_eq!(fs::read_to_string(second).unwrap(), edited);
        assert_eq!(
            scratch.backups(target),
            2,
            "{target}: the original backup and the drift backup"
        );
        check_golden(
            target,
            "drifted.reverted.json",
            &scratch.normalize(&scratch.read(target)),
        );
        assert_eq!(
            scratch.read(target),
            fixture(target, "existing.before.json")
        );
    }
}

#[test]
fn revert_after_user_edit_removes_only_fonsviva_entries() {
    for target in file_targets() {
        let scratch = Scratch::new();
        scratch.seed(target, &fixture(target, "existing.before.json"));
        scratch.wire.apply(target, &endpoint(), false).unwrap();
        let applied = scratch.read(target);
        let edited = applied.replacen('{', "{\n  \"userAddedAfterwards\": {\"keep\": true},", 1);
        fs::write(scratch.path(target), &edited).unwrap();
        assert_eq!(
            scratch.wire.status(target).unwrap().state,
            State::Drifted,
            "{target}: any edit after Fonsviva's write is drift"
        );
        assert_eq!(
            scratch.wire.revert(target, false).unwrap_err().code(),
            "drifted"
        );
        let revert = scratch.wire.revert(target, true).unwrap();
        assert_eq!(revert.outcome, RevertOutcome::RemovedEntries);
        let reverted = scratch.read(target);
        check_golden(
            target,
            "user-edit.reverted.json",
            &scratch.normalize(&reverted),
        );
        assert!(reverted.contains("\"userAddedAfterwards\": {\"keep\": true}"));
        assert!(!reverted.contains("7341"));
    }
}

#[test]
fn reapply_after_user_edit_then_revert_keeps_the_edit() {
    let scratch = Scratch::new();
    let target = Target::Pi;
    scratch.seed(target, &fixture(target, "existing.before.json"));
    scratch.wire.apply(target, &endpoint(), false).unwrap();
    let edited = scratch
        .read(target)
        .replace("\"qwen2.5-coder:7b\"", "\"qwen2.5-coder:14b\"");
    fs::write(scratch.path(target), &edited).unwrap();
    let mut more = endpoint();
    more.models.truncate(1);
    assert_eq!(
        scratch.wire.apply(target, &more, false).unwrap_err().code(),
        "drifted"
    );
    let reapplied = scratch.wire.apply(target, &more, true).unwrap();
    assert_eq!(
        fs::read_to_string(reapplied.drift_backup.unwrap()).unwrap(),
        edited
    );
    assert_eq!(scratch.wire.status(target).unwrap().state, State::Applied);
    let revert = scratch.wire.revert(target, false).unwrap();
    assert_eq!(
        revert.outcome,
        RevertOutcome::RemovedEntries,
        "a stale backup must not overwrite the user's edit"
    );
    let reverted = scratch.read(target);
    assert!(reverted.contains("qwen2.5-coder:14b"));
    assert!(!reverted.contains("fonsviva"));
}

#[test]
fn revert_then_apply_backs_up_the_current_file_again() {
    let scratch = Scratch::new();
    let target = Target::Eukhe;
    scratch.seed(target, &fixture(target, "existing.before.json"));
    let first = scratch.wire.apply(target, &endpoint(), false).unwrap();
    scratch.wire.revert(target, false).unwrap();
    let changed = fixture(target, "existing.before.json").replace("LM Studio", "LM Studio (home)");
    fs::write(scratch.path(target), &changed).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second = scratch.wire.apply(target, &endpoint(), false).unwrap();
    assert_ne!(first.backup, second.backup);
    assert_eq!(fs::read_to_string(second.backup.unwrap()).unwrap(), changed);
}

#[test]
fn user_removed_entry_counts_as_absent_and_gets_a_fresh_backup() {
    let scratch = Scratch::new();
    let target = Target::Pi;
    let before = fixture(target, "existing.before.json");
    scratch.seed(target, &before);
    let first = scratch.wire.apply(target, &endpoint(), false).unwrap();
    let by_hand = before.replace("ollama\"", "ollama-local\"");
    fs::write(scratch.path(target), &by_hand).unwrap();
    assert_eq!(scratch.wire.status(target).unwrap().state, State::Absent);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second = scratch.wire.apply(target, &endpoint(), false).unwrap();
    assert_ne!(first.backup, second.backup);
    scratch.wire.revert(target, false).unwrap();
    assert_eq!(scratch.read(target), by_hand);
}

#[test]
fn t3code_enables_an_existing_pi_instance_and_restores_it() {
    let scratch = Scratch::new();
    let target = Target::T3code;
    let before = fixture(target, "disabled-pi.before.json");
    scratch.seed(target, &before);
    scratch.wire.apply(target, &endpoint(), false).unwrap();
    let applied = scratch.read(target);
    check_golden(
        target,
        "disabled-pi.applied.json",
        &scratch.normalize(&applied),
    );
    assert!(
        !applied.contains("\"pi\": {"),
        "no second pi instance is added"
    );
    assert_eq!(scratch.wire.status(target).unwrap().state, State::Applied);
    scratch.wire.revert(target, false).unwrap();
    assert_eq!(scratch.read(target), before);

    scratch.wire.apply(target, &endpoint(), false).unwrap();
    let edited = scratch.read(target).replace(
        "\"providerInstances\"",
        "\"theme\": \"light\",\n  \"providerInstances\"",
    );
    fs::write(scratch.path(target), &edited).unwrap();
    assert_eq!(
        scratch.wire.revert(target, true).unwrap().outcome,
        RevertOutcome::RemovedEntries
    );
    let reverted = scratch.read(target);
    assert!(reverted.contains("\"enabled\": false"));
    assert!(reverted.contains("\"theme\": \"light\""));
}

#[test]
fn t3code_without_pi_reports_the_install_step_and_writes_nothing() {
    let scratch = Scratch::with_pi(false);
    let before = fixture(Target::T3code, "existing.before.json");
    scratch.seed(Target::T3code, &before);
    let error = scratch
        .wire
        .apply(Target::T3code, &endpoint(), false)
        .unwrap_err();
    assert!(matches!(
        error,
        WireError::NotInstalled { program: "pi", .. }
    ));
    assert_eq!(error.code(), "not_installed");
    assert_eq!(
        error.fix().as_deref(),
        Some("npm install -g --ignore-scripts @earendil-works/pi-coding-agent")
    );
    assert_eq!(scratch.read(Target::T3code), before);
    assert_eq!(scratch.backups(Target::T3code), 0);
}

#[test]
fn not_plain_json_is_refused_untouched() {
    let scratch = Scratch::new();
    let before = "{\n  // my providers\n  \"providers\": {}\n}\n";
    scratch.seed(Target::Pi, before);
    let error = scratch
        .wire
        .apply(Target::Pi, &endpoint(), false)
        .unwrap_err();
    assert_eq!(error.code(), "not_plain_json");
    assert_eq!(scratch.read(Target::Pi), before);
}

#[test]
fn key_goes_to_the_keychain_and_only_a_reference_to_the_file() {
    for target in [Target::Pi, Target::Eukhe] {
        let scratch = Scratch::new();
        let mut with_key = endpoint();
        with_key.api_key = Some(SecretString::from("fv-secret-abc123"));
        let report = scratch.wire.apply(target, &with_key, false).unwrap();
        let written = scratch.read(target);
        assert!(!written.contains("fv-secret-abc123"));
        assert!(!report.diff.contains("fv-secret-abc123"));
        let account = format!("wire-{target}");
        assert!(written.contains(&format!("\"!{}\"", read_command(Os::current(), &account))));
        use fonsviva_wire::KeyStore;
        assert!(scratch.keys.get(&account).unwrap().is_some());
        let journal =
            fs::read_to_string(scratch.root.join("fonsviva-shared/wire/journal.jsonl")).unwrap();
        assert!(!journal.contains("fv-secret-abc123"));
        scratch.wire.revert(target, false).unwrap();
        assert!(
            scratch.keys.get(&account).unwrap().is_none(),
            "revert deletes the key"
        );
    }
}

#[test]
fn openai_target_writes_nothing_and_never_shows_the_key() {
    let scratch = Scratch::new();
    let mut with_key = endpoint();
    with_key.api_key = Some(SecretString::from("fv-secret-openai"));
    let report = scratch
        .wire
        .apply(Target::Openai, &with_key, false)
        .unwrap();
    let setup = report.openai.unwrap();
    assert_eq!(setup.base_url, "http://127.0.0.1:7341/v1");
    for text in [&setup.env, &setup.curl, &setup.key_ref] {
        assert!(!text.contains("fv-secret-openai"));
    }
    assert!(setup.env.contains("OPENAI_BASE_URL"));
    assert!(setup.curl.contains("/chat/completions") && setup.curl.contains("qwen3-4b-q4"));
    assert!(report.path.is_none());
    assert_eq!(
        scratch.wire.status(Target::Openai).unwrap().state,
        State::Applied
    );
    assert_eq!(
        scratch.wire.revert(Target::Openai, false).unwrap().outcome,
        RevertOutcome::Restored
    );
    assert_eq!(
        scratch.wire.status(Target::Openai).unwrap().state,
        State::Absent
    );
}

#[cfg(unix)]
#[test]
fn modes_are_kept_and_new_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let scratch = Scratch::new();
    scratch.wire.apply(Target::Pi, &endpoint(), false).unwrap();
    assert_eq!(mode(&scratch.path(Target::Pi)), 0o600);
    assert_eq!(mode(&scratch.root.join("pi-agent")), 0o700);

    scratch.seed(
        Target::T3code,
        &fixture(Target::T3code, "existing.before.json"),
    );
    fs::set_permissions(
        scratch.path(Target::T3code),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    scratch
        .wire
        .apply(Target::T3code, &endpoint(), false)
        .unwrap();
    assert_eq!(
        mode(&scratch.path(Target::T3code)),
        0o644,
        "existing mode kept"
    );
    scratch.wire.revert(Target::T3code, false).unwrap();
    assert_eq!(mode(&scratch.path(Target::T3code)), 0o644);

    scratch.seed(
        Target::Eukhe,
        &fixture(Target::Eukhe, "existing.before.json"),
    );
    fs::set_permissions(
        scratch.path(Target::Eukhe),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    scratch
        .wire
        .apply(Target::Eukhe, &endpoint(), false)
        .unwrap();
    assert_eq!(
        mode(&scratch.path(Target::Eukhe)),
        0o600,
        "Eukhe's writer always uses 0600, like Local Studio's"
    );
}

#[cfg(unix)]
#[test]
fn symlinked_config_is_written_through_the_link() {
    let scratch = Scratch::new();
    let real = scratch.root.join("dotfiles-models.json");
    fs::write(&real, fixture(Target::Pi, "existing.before.json")).unwrap();
    fs::create_dir_all(scratch.root.join("pi-agent")).unwrap();
    std::os::unix::fs::symlink(&real, scratch.path(Target::Pi)).unwrap();
    scratch.wire.apply(Target::Pi, &endpoint(), false).unwrap();
    assert!(
        fs::symlink_metadata(scratch.path(Target::Pi))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(fs::read_to_string(&real).unwrap().contains("\"fonsviva\""));
}

fn env_for(os: Os, pairs: &[(&str, &str)]) -> WireEnv {
    WireEnv {
        os,
        vars: pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect(),
        shared_dir: PathBuf::from("state"),
    }
}

#[test]
fn windows_paths_use_userprofile_and_backslashes() {
    let env = env_for(
        Os::Windows,
        &[
            ("USERPROFILE", "C:\\Users\\Ann"),
            ("HOME", "C:\\msys\\home\\ann"),
        ],
    );
    assert_eq!(
        env.pi_agent_dir().unwrap(),
        PathBuf::from("C:\\Users\\Ann\\.pi\\agent")
    );
    assert_eq!(
        env.t3_settings_path().unwrap(),
        PathBuf::from("C:\\Users\\Ann\\.t3\\userdata\\settings.json")
    );
    assert_eq!(
        env.eukhe_agent_dir().unwrap(),
        PathBuf::from("C:\\msys\\home\\ann\\.eukhe"),
        "Eukhe reads HOME only"
    );

    let env = env_for(
        Os::Windows,
        &[
            ("HOMEDRIVE", "D:"),
            ("HOMEPATH", "\\Users\\Bo\\"),
            ("T3CODE_HOME", "~\\T3"),
        ],
    );
    assert_eq!(
        env.t3_settings_path().unwrap(),
        PathBuf::from("D:\\Users\\Bo\\T3\\userdata\\settings.json")
    );
    assert_eq!(
        env.pi_agent_dir().unwrap(),
        PathBuf::from("D:\\Users\\Bo\\.pi\\agent")
    );
    assert!(matches!(
        env.eukhe_agent_dir(),
        Err(WireError::NoHome("HOME"))
    ));

    let env = env_for(Os::Windows, &[("PI_CODING_AGENT_DIR", "E:\\pi\\agent\\")]);
    assert_eq!(
        env.pi_agent_dir().unwrap(),
        PathBuf::from("E:\\pi\\agent\\")
    );
    assert!(env.t3_settings_path().is_err());
}

#[test]
fn unix_paths_and_overrides() {
    let env = env_for(Os::MacOs, &[("HOME", "/Users/ann")]);
    assert_eq!(
        env.pi_agent_dir().unwrap(),
        PathBuf::from("/Users/ann/.pi/agent")
    );
    assert_eq!(
        env.t3_settings_path().unwrap(),
        PathBuf::from("/Users/ann/.t3/userdata/settings.json")
    );
    assert_eq!(
        env.eukhe_agent_dir().unwrap(),
        PathBuf::from("/Users/ann/.eukhe")
    );
    let env = env_for(
        Os::Linux,
        &[
            ("HOME", "/home/ann"),
            ("PI_CODING_AGENT_DIR", "~/alt/pi"),
            ("EUKHE_CODING_AGENT_DIR", "~"),
            ("T3CODE_HOME", "/srv/t3"),
        ],
    );
    assert_eq!(
        env.pi_agent_dir().unwrap(),
        PathBuf::from("/home/ann/alt/pi")
    );
    assert_eq!(env.eukhe_agent_dir().unwrap(), PathBuf::from("/home/ann"));
    assert_eq!(
        env.t3_settings_path().unwrap(),
        PathBuf::from("/srv/t3/userdata/settings.json")
    );
    assert_eq!(
        read_command(Os::Linux, "wire-pi"),
        "secret-tool lookup service fonsviva username wire-pi"
    );
    assert_eq!(
        read_command(Os::MacOs, "wire-pi"),
        "security find-generic-password -s fonsviva -a wire-pi -w"
    );
}
