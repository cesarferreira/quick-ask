use crate::chat::Message;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const MAX_CONVERSATIONS: usize = 100;
const MAX_HISTORY_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub model: String,
    pub messages: Vec<Message>,
    pub updated_at: u64,
}

impl Conversation {
    pub fn new(model: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            title: "New chat".into(),
            model,
            messages: Vec::new(),
            updated_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }
}

pub struct HistoryStore {
    data_dir: PathBuf,
}

impl HistoryStore {
    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }
    pub fn load(&self) -> Result<Vec<Conversation>> {
        let path = self.data_dir.join("history.json");
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("Could not open local chat history."),
        };
        let mut bytes = Vec::new();
        file.take(MAX_HISTORY_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("Could not read local chat history.")?;
        if bytes.len() as u64 > MAX_HISTORY_BYTES {
            bail!("Local chat history is too large. The original history was preserved.");
        }
        let mut conversations: Vec<Conversation> = serde_json::from_slice(&bytes).context(
            "Local chat history is damaged or truncated. The original history was preserved.",
        )?;
        conversations.sort_by_key(|conversation| std::cmp::Reverse(conversation.updated_at));
        conversations.truncate(MAX_CONVERSATIONS);
        Ok(conversations)
    }

    pub fn save(&self, conversations: &[Conversation]) -> Result<()> {
        // A failed load must never turn a damaged file into an empty successful save.
        self.load()?;
        let mut recent: Vec<_> = conversations.iter().collect();
        recent.sort_by_key(|conversation| std::cmp::Reverse(conversation.updated_at));
        recent.truncate(MAX_CONVERSATIONS);
        let bytes = serde_json::to_vec(&recent).context("Could not encode local chat history.")?;
        if bytes.len() as u64 > MAX_HISTORY_BYTES {
            bail!("Local chat history is too large to save.");
        }
        fs::create_dir_all(&self.data_dir)
            .context("Could not create the chat history directory.")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.data_dir, fs::Permissions::from_mode(0o700))
                .context("Could not protect the chat history directory.")?;
        }
        let temporary = self
            .data_dir
            .join(format!(".history-{}.tmp", Uuid::new_v4()));
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temporary)
                .context("Could not create a private chat history file.")?;
            file.write_all(&bytes)
                .context("Could not write local chat history.")?;
            file.sync_all()
                .context("Could not flush local chat history.")?;
            fs::rename(&temporary, self.data_dir.join("history.json"))
                .context("Could not replace local chat history atomically.")?;
            File::open(&self.data_dir)?
                .sync_all()
                .context("Could not flush the chat history directory.")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn conversation(title: &str, updated_at: u64) -> Conversation {
        Conversation {
            id: format!("id-{updated_at}"),
            title: title.into(),
            model: "fast".into(),
            messages: vec![Message {
                role: "user".into(),
                content: "café 🙂".into(),
            }],
            updated_at,
        }
    }

    #[test]
    fn missing_history_is_empty_without_creating_files() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("not-created");
        assert!(HistoryStore::new(path.clone()).load().unwrap().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn saved_history_round_trips_messages_and_returns_newest_first() {
        let directory = tempdir().unwrap();
        let store = HistoryStore::new(directory.path().join("data"));
        store
            .save(&[conversation("Older", 10), conversation("Recent", 20)])
            .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(
            loaded.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(),
            vec!["Recent", "Older"]
        );
        assert_eq!(loaded[0].messages[0].content, "café 🙂");
        assert_eq!(loaded[0].model, "fast");
        store.save(&[conversation("Replacement", 30)]).unwrap();
        assert_eq!(store.load().unwrap()[0].title, "Replacement");
        assert_eq!(
            fs::read_dir(directory.path().join("data")).unwrap().count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn history_directory_and_atomic_file_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempdir().unwrap();
        let path = directory.path().join("data");
        let store = HistoryStore::new(path.clone());
        store.save(&[conversation("Private", 1)]).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(path.join("history.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        store.save(&[conversation("Still private", 2)]).unwrap();
        assert_eq!(
            fs::metadata(path.join("history.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn corrupt_history_is_reported_and_preserved_when_saving() {
        let directory = tempdir().unwrap();
        let original = b"[{\"id\":\"truncated";
        let path = directory.path().join("history.json");
        fs::write(&path, original).unwrap();
        let store = HistoryStore::new(directory.path().into());
        assert!(store.load().unwrap_err().to_string().contains("history"));
        assert!(store.save(&[conversation("New", 1)]).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn save_retains_only_the_hundred_most_recent_conversations() {
        let directory = tempdir().unwrap();
        let store = HistoryStore::new(directory.path().into());
        let entries: Vec<_> = (0..125).map(|i| conversation("Chat", i)).collect();
        store.save(&entries).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 100);
        assert_eq!(loaded.first().unwrap().updated_at, 124);
        assert_eq!(loaded.last().unwrap().updated_at, 25);
    }
}
