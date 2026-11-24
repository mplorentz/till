//! Integration tests for the `till-card` operator command (ADR 0007).
//!
//! The interactive TTY prompts are not automatable in CI, so the tests drive
//! the `--from-stdin` path (same code after input collection) and assert the
//! two properties that matter: the card lands as ciphertext with a clear
//! pet name and last four, and card values are refused as arguments (argv is a
//! `ps`/history scrape vector).
//!
//! Synthetic digits are built at runtime: no source file contains a digit run
//! (US-14, `script/no-card-digits`).

use std::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use broker::{CardCipher, SqliteStore};

type TestResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const BINARY: &str = env!("CARGO_BIN_EXE_till-card");
const PET_NAME: &str = "Groceries";
const PAN: &str = "4444";
const LAST4: &str = "4444";

fn temp_dir(tag: &str) -> TestResult<PathBuf> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("till-card-cli-{tag}-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn database(dir: &Path) -> PathBuf {
    dir.join("till.db")
}

fn identity(dir: &Path) -> PathBuf {
    dir.join("age-identity.txt")
}

/// Synthetic card values, built at runtime.
fn pan() -> String {
    format!("{}{}", "7".repeat(12), PAN)
}

fn card_lines() -> String {
    format!(
        "{PET_NAME}\n{}\n12/34\n{}\nTest Holder\n{}\n",
        pan(),
        "1".repeat(3),
        "1".repeat(5)
    )
}

fn run(dir: &Path, arguments: &[&str], stdin: Option<&str>) -> TestResult<(bool, String)> {
    let mut command = Command::new(BINARY);
    command.args(arguments);
    command
        .arg("--db")
        .arg(database(dir))
        .arg("--identity")
        .arg(identity(dir));
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    match stdin {
        Some(input) => {
            command.stdin(Stdio::piped());
            let mut child = command.spawn()?;
            child
                .stdin
                .take()
                .ok_or("stdin")?
                .write_all(input.as_bytes())?;
            let output = child.wait_with_output()?;
            Ok((
                output.status.success(),
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ),
            ))
        }
        None => {
            command.stdin(Stdio::null());
            let output = command.output()?;
            Ok((
                output.status.success(),
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ),
            ))
        }
    }
}

#[test]
fn cli_add_reads_a_card_from_stdin_and_stores_ciphertext_and_last4() -> TestResult<()> {
    let dir = temp_dir("add")?;

    // `run` prepends `--db/--identity`; `add` is the subcommand under test.
    let mut command = Command::new(BINARY);
    command
        .arg("add")
        .arg("--db")
        .arg(database(&dir))
        .arg("--identity")
        .arg(identity(&dir))
        .arg("--from-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    child
        .stdin
        .take()
        .ok_or("stdin")?
        .write_all(card_lines().as_bytes())?;
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "the card must be registered: {stdout}{stderr}"
    );
    assert!(
        stdout.contains(PET_NAME) && stdout.contains(LAST4),
        "the confirmation names the pet name and last four"
    );
    assert!(
        !stdout.contains(&pan()),
        "the CLI never echoes the card number"
    );

    // The vault is readable, and holds the card as ciphertext.
    let store = SqliteStore::open(database(&dir))?;
    let cards = store.cards()?;
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].pet_name, PET_NAME);
    assert_eq!(cards[0].last4, LAST4);

    let blob = store.card_blob(cards[0].card_id)?.ok_or("stored blob")?;
    let armored = String::from_utf8(blob)?;
    assert!(armored.starts_with("-----BEGIN AGE ENCRYPTED FILE-----"));
    assert!(!armored.contains(&pan()));

    // The identity was generated on first use, on the data volume.
    assert!(identity(&dir).exists(), "the key is created when absent");
    let cipher = CardCipher::load_or_generate(identity(&dir))?;
    let decrypted = cipher.decrypt(&store.card_blob(cards[0].card_id)?.ok_or("stored blob")?)?;
    assert_eq!(decrypted.fields().card_number, pan());

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

#[test]
fn cli_refuses_card_values_as_command_line_arguments() -> TestResult<()> {
    let dir = temp_dir("argv")?;

    let (success, output) = run(&dir, &["add", "--card-number", &pan()], None)?;

    assert!(!success, "card values must never be accepted as arguments");
    assert!(
        output.contains("usage"),
        "the refusal explains the interface: {output}"
    );
    // Nothing was written, not even a vault row.
    let store = SqliteStore::open(database(&dir))?;
    assert!(store.cards()?.is_empty(), "a refused add writes nothing");

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

#[test]
fn cli_list_shows_pet_name_and_last4_only() -> TestResult<()> {
    let dir = temp_dir("list")?;
    let mut add = Command::new(BINARY);
    add.arg("add")
        .arg("--db")
        .arg(database(&dir))
        .arg("--identity")
        .arg(identity(&dir))
        .arg("--from-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = add.spawn()?;
    child
        .stdin
        .take()
        .ok_or("stdin")?
        .write_all(card_lines().as_bytes())?;
    assert!(child.wait_with_output()?.status.success());

    let (success, output) = run(&dir, &["list"], None)?;

    assert!(success, "list succeeds: {output}");
    assert!(output.contains(PET_NAME));
    assert!(output.contains(LAST4), "the human needs the last four");
    assert!(
        !output.contains(&pan()),
        "the listing never shows a full card number"
    );

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

#[test]
fn cli_remove_deletes_a_card() -> TestResult<()> {
    let dir = temp_dir("remove")?;
    let mut add = Command::new(BINARY);
    add.arg("add")
        .arg("--db")
        .arg(database(&dir))
        .arg("--identity")
        .arg(identity(&dir))
        .arg("--from-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = add.spawn()?;
    child
        .stdin
        .take()
        .ok_or("stdin")?
        .write_all(card_lines().as_bytes())?;
    assert!(child.wait_with_output()?.status.success());

    let store = SqliteStore::open(database(&dir))?;
    let card_id = store.cards()?[0].card_id;

    let (success, output) = run(&dir, &["remove", &card_id.to_string()], None)?;

    assert!(success, "remove succeeds: {output}");
    assert!(store.cards()?.is_empty(), "the card is gone");

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
