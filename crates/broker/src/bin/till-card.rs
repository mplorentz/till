//! `till-card` — the operator's card-vault command (ADR 0007).
//!
//! One command per card, run inside the broker container:
//!
//! ```text
//! docker exec -it till till-card add
//! docker exec -it till till-card list
//! docker exec -it till till-card remove <card-id>
//! ```
//!
//! `add` prompts for the pet name and the five card fields. The sensitive
//! values (number, expiry, CVV) are read from the terminal with echo disabled;
//! **no card value is ever accepted as a command-line argument**, because argv
//! is visible in `ps` and in shell history. Nothing is logged: the only output
//! mentions the pet name and the last four digits, both of which are display
//! data (AGENTS.md rules 1 and 6, ADR 0002).
//!
//! The card is encrypted with the deployment's age identity before it touches
//! SQLite. The identity lives on the data volume and is created on first use —
//! the same `load_or_generate` the broker itself calls, so the CLI and the
//! broker always share one key.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use broker::crypto::{CardCipher, CardRecord, IDENTITY_ENV_VAR, IDENTITY_FILE_NAME};
use broker::{CardId, SqliteStore};

/// Environment variable naming the database (deployment default below).
const DB_ENV_VAR: &str = "TILL_DB";
/// Where the broker keeps its database inside the container.
const DEFAULT_DB_PATH: &str = "/data/till.db";

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            // Static text only: no card field can reach this line.
            eprintln!("till-card: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: Vec<String>) -> Result<(), &'static str> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return Err(USAGE);
    };

    match command {
        "add" => add(&arguments[1..]),
        "list" => list(&arguments[1..]),
        "remove" => remove(&arguments[1..]),
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        _ => Err(USAGE),
    }
}

const USAGE: &str = "usage:
  till-card add [--db PATH] [--identity PATH] [--from-stdin]
  till-card list [--db PATH]
  till-card remove <card-id> [--db PATH]

Card values are never accepted as arguments; `add` prompts for them
(`--from-stdin` reads six lines instead: pet name, number, expiry, cvv,
cardholder name, billing postcode).";

/// Flags shared by every subcommand.
#[derive(Default)]
struct Options {
    db: Option<String>,
    identity: Option<String>,
    from_stdin: bool,
}

/// Parse only the flags this CLI knows.
///
/// Anything else — including anything that looks like a card value — is a
/// usage error, so `till-card add --pan ...` cannot be made to work by
/// accident and cannot put digits in `ps`.
fn parse_options(arguments: &[String]) -> Result<Options, &'static str> {
    let mut options = Options::default();
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        index += 1;
        match argument {
            "--from-stdin" => options.from_stdin = true,
            "--db" => {
                let value = arguments.get(index).ok_or(USAGE)?;
                index += 1;
                options.db = Some(value.clone());
            }
            "--identity" => {
                let value = arguments.get(index).ok_or(USAGE)?;
                index += 1;
                options.identity = Some(value.clone());
            }
            _ => return Err(USAGE),
        }
    }
    Ok(options)
}

/// The database to write to: `--db`, then `TILL_DB`, then the container path.
fn database_path(options: &Options) -> String {
    options
        .db
        .clone()
        .or_else(|| std::env::var(DB_ENV_VAR).ok())
        .unwrap_or_else(|| DEFAULT_DB_PATH.to_string())
}

/// The age identity: `--identity`, then `TILL_AGE_IDENTITY`, then the data
/// volume next to the database. Missing is fine — it is generated once.
fn identity_path(options: &Options, database: &str) -> String {
    if let Some(path) = options.identity.clone() {
        return path;
    }
    if let Ok(path) = std::env::var(IDENTITY_ENV_VAR) {
        return path;
    }
    let directory = std::path::Path::new(database)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    directory.join(IDENTITY_FILE_NAME).to_string_lossy().into()
}

fn add(arguments: &[String]) -> Result<(), &'static str> {
    let options = parse_options(arguments)?;
    let database = database_path(&options);
    let identity = identity_path(&options, &database);

    let cipher = CardCipher::load_or_generate(&identity).map_err(|error| match error {
        broker::CryptoError::KeyLoad => "the card key file could not be read",
        broker::CryptoError::KeyWrite => "the card key file could not be written",
        _ => "card encryption is unavailable",
    })?;

    let input = if options.from_stdin {
        read_batch()?
    } else {
        read_interactive()?
    };

    let card = CardRecord::parse(
        &input.card_number,
        &input.expiry,
        &input.cvv,
        &input.cardholder_name,
        &input.billing_postcode,
    )
    .map_err(|error| match error {
        // Field *kind* only — never the value.
        broker::CryptoError::MalformedCard(field) => match field {
            "card_number" => "card number must be 12-19 digits",
            "expiry" => "expiry must be 1-10 characters (e.g. MM/YY)",
            "cvv" => "cvv must be 3-4 digits",
            "cardholder_name" => "cardholder name must be 1-64 characters",
            "billing_postcode" => "billing postcode must be 1-16 characters",
            _ => "invalid card field",
        },
        _ => "the card could not be encrypted",
    })?;

    let store = SqliteStore::open(&database).map_err(|_| "the database could not be opened")?;
    let summary = store
        .add_card(&cipher, &input.pet_name, &card)
        .map_err(|error| match error {
            broker::StoreError::InvalidCardName("must not be empty") => {
                "pet name must not be empty"
            }
            broker::StoreError::InvalidCardName(_) => "pet name must be at most 48 bytes",
            broker::StoreError::DuplicateCardName(_) => "that pet name is already in use",
            _ => "the card could not be stored",
        })?;

    // Display data only: pet name and last four (AGENTS.md rule 6).
    println!(
        "added card {:?} (last four {}) — id {}",
        summary.pet_name, summary.last4, summary.card_id
    );
    Ok(())
}

fn list(arguments: &[String]) -> Result<(), &'static str> {
    let options = parse_options(arguments)?;
    if options.from_stdin {
        return Err(USAGE);
    }
    let database = database_path(&options);
    let store = SqliteStore::open(&database).map_err(|_| "the database could not be opened")?;
    let cards = store.cards().map_err(|_| "the cards could not be read")?;

    if cards.is_empty() {
        println!("no cards registered — run `till-card add`");
        return Ok(());
    }
    for card in cards {
        println!(
            "{}  {:24}  last four {}",
            card.card_id, card.pet_name, card.last4
        );
    }
    Ok(())
}

fn remove(arguments: &[String]) -> Result<(), &'static str> {
    let (id, rest) = arguments.split_first().ok_or(USAGE)?;
    let card_id: CardId = id.parse().map_err(|_| "malformed card id")?;
    let options = parse_options(rest)?;
    let database = database_path(&options);

    let store = SqliteStore::open(&database).map_err(|_| "the database could not be opened")?;
    let retired = store.retire_card(card_id).map_err(|error| match error {
        broker::StoreError::CardInUse(_) => {
            "that card is in use by a payment that has not settled yet"
        }
        _ => "the card could not be retired",
    })?;
    if retired {
        // Retirement keeps the row so payments keep their history; the card
        // simply stops being offered.
        println!("retired card {card_id}");
        Ok(())
    } else {
        Err("no such card, or it is already retired")
    }
}

/// The six values `add` collects, straight from the terminal buffer.
struct CardInput {
    pet_name: String,
    card_number: String,
    expiry: String,
    cvv: String,
    cardholder_name: String,
    billing_postcode: String,
}

/// Interactive prompts: sensitive fields without echo, the rest with it.
fn read_interactive() -> Result<CardInput, &'static str> {
    let pet_name = prompt_visible("Pet name for this card: ")?;
    let card_number = prompt_secret("Card number: ")?;
    let expiry = prompt_secret("Expiry (MM/YY): ")?;
    let cvv = prompt_secret("CVV: ")?;
    let cardholder_name = prompt_visible("Cardholder name: ")?;
    let billing_postcode = prompt_visible("Billing postcode: ")?;

    Ok(CardInput {
        pet_name,
        card_number,
        expiry,
        cvv,
        cardholder_name,
        billing_postcode,
    })
}

/// Scripted input: six plain lines. Used by `--from-stdin` and the tests;
/// the caller is responsible for not putting digits in a shell history.
fn read_batch() -> Result<CardInput, &'static str> {
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut next = || -> Result<String, &'static str> {
        lines
            .next()
            .transpose()
            .map_err(|_| "could not read input")?
            .ok_or("expected six lines: pet name, number, expiry, cvv, cardholder name, billing postcode")
    };

    Ok(CardInput {
        pet_name: next()?,
        card_number: next()?,
        expiry: next()?,
        cvv: next()?,
        cardholder_name: next()?,
        billing_postcode: next()?,
    })
}

fn prompt_visible(prompt: &str) -> Result<String, &'static str> {
    print!("{prompt}");
    io::stdout().flush().map_err(|_| "could not write prompt")?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|_| "could not read input")?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

fn prompt_secret(prompt: &str) -> Result<String, &'static str> {
    rpassword::prompt_password(prompt).map_err(|_| "could not read input")
}
