use anki_decks::{cards_for_deck, deck_names, set_mp3_audio, text_to_speech, Card, Side};
use base64::Engine as _;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "anki-decks")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    ListDecks,
    ListCards { deck: String },
    Tts {
        #[arg(long)]
        api_key: String,
        text: String,
    },
    SetMp3Audio {
        #[arg(long, value_enum)]
        side: CliSide,
        #[arg(long)]
        card: i64,
        #[arg(long)]
        audio_base64: String,
    },
}

/// Clap-facing mirror of `Side`; kept separate so the library has no clap dependency.
#[derive(Clone, ValueEnum)]
enum CliSide {
    Front,
    Back,
}

impl From<CliSide> for Side {
    fn from(s: CliSide) -> Self {
        match s {
            CliSide::Front => Side::Front,
            CliSide::Back => Side::Back,
        }
    }
}

fn anki_collection_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let base = PathBuf::from(home).join("Library/Application Support/Anki2");

    for entry in std::fs::read_dir(&base).ok()? {
        let entry = entry.ok()?;
        let path = entry.path().join("collection.anki2");
        if path.exists() {
            return Some(path);
        }
    }
    None
}

fn anki_path() -> PathBuf {
    anki_collection_path().unwrap_or_else(|| {
        eprintln!("Could not find Anki collection. Is Anki installed?");
        std::process::exit(1);
    })
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::ListDecks => {
            let names = deck_names(&anki_path()).unwrap_or_else(|e| {
                eprintln!("Failed to read decks: {e}");
                std::process::exit(1);
            });
            println!("{}", serde_json::to_string_pretty(&names).unwrap());
        }
        Command::ListCards { deck } => {
            let rx = cards_for_deck(&anki_path(), &deck).unwrap_or_else(|e| {
                eprintln!("Failed to open collection: {e}");
                std::process::exit(1);
            });

            for Card { card_id, note_id, front, back } in rx {
                println!("{}", serde_json::json!({
                    "card_id": card_id,
                    "note_id": note_id,
                    "front": front,
                    "back": back,
                }));
            }
        }
        Command::Tts { api_key, text } => {
            let audio = text_to_speech(&api_key, &text).unwrap_or_else(|e| {
                eprintln!("TTS request failed: {e}");
                std::process::exit(1);
            });
            let encoded = base64::engine::general_purpose::STANDARD.encode(&audio.data);
            println!("{}", serde_json::json!({
                "audio_type": audio.audio_type,
                "audio": encoded,
            }));
        }
        Command::SetMp3Audio { side, card, audio_base64 } => {
            let audio_data = base64::engine::general_purpose::STANDARD
                .decode(&audio_base64)
                .unwrap_or_else(|e| {
                    eprintln!("Invalid base64: {e}");
                    std::process::exit(1);
                });

            set_mp3_audio(&anki_path(), card, side.into(), &audio_data).unwrap_or_else(|e| {
                eprintln!("Failed to set audio: {e}");
                std::process::exit(1);
            });

            println!("{}", serde_json::json!({ "ok": true, "card_id": card }));
        }
    }
}
