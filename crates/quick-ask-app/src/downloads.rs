use anyhow::{Context as _, Result, bail};
use std::{
    fs::OpenOptions,
    io::{ErrorKind, Write as _},
    path::{Path, PathBuf},
};

const MAX_STEM_CHARS: usize = 80;
const MAX_ATTEMPTS: usize = 1_000;

/// Writes image bytes into `directory` without replacing an existing file.
pub fn save_image(directory: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
    let extension = image::guess_format(bytes)
        .context("unsupported image format")?
        .extensions_str()
        .first()
        .copied()
        .unwrap_or("img");
    let stem = file_stem(name);
    for attempt in 0..MAX_ATTEMPTS {
        let file_name = if attempt == 0 {
            format!("{stem}.{extension}")
        } else {
            format!("{stem} ({attempt}).{extension}")
        };
        let path = directory.join(file_name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(error).context("writing image");
                }
                return Ok(path);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("creating image file"),
        }
    }
    bail!("too many files named {stem}")
}

/// A display name or URL reduced to a safe file stem without its image extension.
fn file_stem(name: &str) -> String {
    let name = name
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    let name = match name.rsplit_once('.') {
        Some((stem, extension))
            if matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff"
            ) =>
        {
            stem
        }
        _ => name,
    };
    let stem: String = name
        .chars()
        .filter(|c| !c.is_control() && *c != ':')
        .take(MAX_STEM_CHARS)
        .collect();
    let stem = stem.trim().trim_start_matches('.').trim();
    if stem.is_empty() {
        "Image".into()
    } else {
        stem.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, b'I', b'H', b'D', b'R',
    ];

    #[test]
    fn stems_drop_paths_queries_and_image_extensions() {
        assert_eq!(file_stem("https://example.com/a/cat.PNG?size=2#x"), "cat");
        assert_eq!(file_stem("../../etc/passwd"), "passwd");
        assert_eq!(file_stem("Screenshot 10:30.jpeg"), "Screenshot 1030");
        assert_eq!(file_stem("notes.v2"), "notes.v2");
        assert_eq!(file_stem(".."), "Image");
        assert_eq!(file_stem(""), "Image");
    }

    #[test]
    fn saving_uses_the_detected_format_and_never_overwrites() {
        let directory = std::env::temp_dir().join(format!(
            "quick-ask-downloads-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let first = save_image(&directory, "Cat.jpg", PNG).unwrap();
        let second = save_image(&directory, "Cat.jpg", PNG).unwrap();
        assert_eq!(first.file_name().unwrap(), "Cat.png");
        assert_eq!(second.file_name().unwrap(), "Cat (1).png");
        assert_eq!(std::fs::read(&first).unwrap(), PNG);
        assert!(save_image(&directory, "Cat", b"not an image").is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
