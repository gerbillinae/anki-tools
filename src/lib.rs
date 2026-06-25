use base64::Engine as _;
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::sync::mpsc::{sync_channel, Receiver};

const TTS_URL: &str = "https://texttospeech.googleapis.com/v1/text:synthesize";

pub struct TtsAudio {
    pub audio_type: &'static str,
    pub data: Vec<u8>,
}

/// Returns the raw audio bytes and type for `text` using Google Cloud TTS.
pub fn text_to_speech(api_key: &str, text: &str) -> Result<TtsAudio, Box<dyn std::error::Error>> {
    let body = serde_json::json!({
        "input": { "text": text },
        "voice": { "languageCode": "en-US", "ssmlGender": "NEUTRAL" },
        "audioConfig": { "audioEncoding": "MP3" },
    });

    let response: serde_json::Value = ureq::post(TTS_URL)
        .query("key", api_key)
        .send_json(body)?
        .body_mut()
        .read_json()?;

    let encoded = response["audioContent"]
        .as_str()
        .ok_or("missing audioContent in response")?;

    let data = base64::engine::general_purpose::STANDARD.decode(encoded)?;

    Ok(TtsAudio { audio_type: "mp3", data })
}

pub struct Card {
    pub card_id: i64,
    pub note_id: i64,
    pub front: String,
    pub back: String,
}

#[derive(Clone, Copy)]
pub enum Side {
    Front,
    Back,
}

impl Side {
    /// Field ordinal within the note's flds array.
    fn field_ord(self) -> usize {
        match self {
            Side::Front => 0,
            Side::Back => 1,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Side::Front => "front",
            Side::Back => "back",
        }
    }
}

fn open_ro(db_path: &Path) -> Result<Connection, rusqlite::Error> {
    Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

fn open_rw(db_path: &Path) -> Result<Connection, rusqlite::Error> {
    Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

pub fn deck_names(db_path: &Path) -> Result<Vec<String>, rusqlite::Error> {
    let conn = open_ro(db_path)?;
    let mut stmt = conn.prepare("SELECT name FROM decks")?;

    let mut names: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;

    names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    Ok(names)
}

/// Returns a channel receiver that yields every card in `deck_name`.
/// Cards are sent one at a time as they are read from the database.
/// The channel buffer is bounded so the producer won't race far ahead of the consumer.
pub fn cards_for_deck(
    db_path: &Path,
    deck_name: &str,
) -> Result<Receiver<Card>, rusqlite::Error> {
    let conn = open_ro(db_path)?;

    // decks.name is declared COLLATE unicase, which rusqlite doesn't have registered,
    // so we can't use it in a WHERE clause. Instead, fetch all (id, name) pairs and
    // match in Rust, then query cards by deck id (which has an index).
    let deck_id: i64 = {
        let mut stmt = conn.prepare("SELECT id, name FROM decks")?;
        let mut rows = stmt.query([])?;
        let mut found = None;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let name: String = row.get(1)?;
            if name.eq_ignore_ascii_case(deck_name) {
                found = Some(id);
                break;
            }
        }
        match found {
            Some(id) => id,
            None => {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
        }
    };

    // Buffer of 64 cards; the producer blocks when the consumer falls behind.
    let (tx, rx) = sync_channel::<Card>(64);

    std::thread::spawn(move || {
        let mut stmt = match conn.prepare(
            "SELECT c.id, c.nid, n.flds \
             FROM cards c \
             JOIN notes n ON c.nid = n.id \
             WHERE c.did = ?1",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };

        let mut rows = match stmt.query([deck_id]) {
            Ok(r) => r,
            Err(_) => return,
        };

        while let Ok(Some(row)) = rows.next() {
            let (card_id, note_id, flds): (i64, i64, String) = match (
                row.get(0),
                row.get(1),
                row.get(2),
            ) {
                (Ok(c), Ok(n), Ok(f)) => (c, n, f),
                _ => continue,
            };
            // Anki separates fields with the unit-separator character (0x1f).
            let mut parts = flds.splitn(2, '\x1f');
            let front = parts.next().unwrap_or("").to_string();
            let back = parts.next().unwrap_or("").to_string();
            if tx.send(Card { card_id, note_id, front, back }).is_err() {
                break;
            }
        }
    });

    Ok(rx)
}

/// Attaches an MP3 to one side of a card.
///
/// Two writes are performed:
///   1. The audio bytes are written to `collection.media/` as
///      `anki_decks_{card_id}_{side}.mp3`.
///   2. The note's `flds` column is updated to embed `[sound:<filename>]`
///      in the appropriate field. Any previous sound tag written by this
///      tool on that field is replaced. The note's `mod` and `usn` columns
///      are updated so Anki treats it as locally modified and queues it for
///      sync on next AnkiWeb connection.
pub fn set_mp3_audio(
    db_path: &Path,
    card_id: i64,
    side: Side,
    audio_data: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    // --- Read phase: look up the note and validate the card exists ----------
    let (note_id, current_flds) = {
        let conn = open_ro(db_path)?;
        let mut stmt = conn.prepare(
            "SELECT n.id, n.flds \
             FROM cards c \
             JOIN notes n ON c.nid = n.id \
             WHERE c.id = ?1",
        )?;
        let mut rows = stmt.query([card_id])?;
        let row = rows.next()?.ok_or("card not found")?;
        let note_id: i64 = row.get(0)?;
        let flds: String = row.get(1)?;
        (note_id, flds)
    };

    // --- Build updated flds -------------------------------------------------
    let field_ord = side.field_ord();
    let mut fields: Vec<String> = current_flds.split('\x1f').map(str::to_string).collect();

    if field_ord >= fields.len() {
        return Err(format!(
            "note has {} field(s); field ordinal {} ({}) is out of range",
            fields.len(),
            field_ord,
            side.as_str(),
        )
        .into());
    }

    let filename = format!("anki_decks_{}_{}.mp3", card_id, side.as_str());
    let sound_tag = format!("[sound:{filename}]");

    // Strip any previous sound tag written by this tool, then append the new one.
    let field_text = fields[field_ord]
        .replace(&sound_tag, "")
        .trim_end()
        .to_string();
    fields[field_ord] = format!("{field_text} {sound_tag}").trim_start().to_string();

    let new_flds = fields.join("\x1f");

    // --- Write the media file -----------------------------------------------
    let media_dir = db_path
        .parent()
        .ok_or("could not determine collection directory")?
        .join("collection.media");
    std::fs::write(media_dir.join(&filename), audio_data)?;

    // --- Write the updated note to the DB -----------------------------------
    let mod_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;

    let conn = open_rw(db_path)?;
    conn.execute(
        "UPDATE notes SET flds = ?1, mod = ?2, usn = -1 WHERE id = ?3",
        rusqlite::params![new_flds, mod_ts, note_id],
    )?;

    Ok(())
}
