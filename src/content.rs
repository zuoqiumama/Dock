use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentKind {
    Application,
    Shortcut,
    Folder,
    Image,
    File,
}

pub fn classify_path(path: &Path) -> ContentKind {
    if path.is_dir() {
        return ContentKind::Folder;
    }

    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "exe" | "com" | "bat" | "cmd" => ContentKind::Application,
        "lnk" | "url" => ContentKind::Shortcut,
        "bmp" | "dib" | "gif" | "heic" | "heif" | "ico" | "jfif" | "jpe" | "jpeg" | "jpg"
        | "png" | "tif" | "tiff" | "webp" => ContentKind::Image,
        _ => ContentKind::File,
    }
}

pub fn fallback_visual(kind: ContentKind) -> (&'static str, (f32, f32, f32)) {
    match kind {
        ContentKind::Application => ("\u{25A3}", (0.22, 0.48, 0.82)),
        ContentKind::Shortcut => ("\u{2197}", (0.30, 0.58, 0.82)),
        ContentKind::Folder => ("\u{1F4C1}", (0.93, 0.66, 0.20)),
        ContentKind::Image => ("\u{1F5BC}", (0.48, 0.64, 0.46)),
        ContentKind::File => ("\u{1F4C4}", (0.52, 0.54, 0.60)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("featherdock-content-{nonce}-{name}"))
    }

    #[test]
    fn classifies_supported_content_types() {
        let dir = temp_path("folder");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(classify_path(&dir), ContentKind::Folder);
        fs::remove_dir_all(&dir).unwrap();

        assert_eq!(
            classify_path(Path::new(r"C:\Tools\app.exe")),
            ContentKind::Application
        );
        assert_eq!(
            classify_path(Path::new(r"C:\Links\site.url")),
            ContentKind::Shortcut
        );
        assert_eq!(
            classify_path(Path::new(r"C:\Pictures\photo.PNG")),
            ContentKind::Image
        );
        assert_eq!(
            classify_path(Path::new(r"C:\Docs\manual.pdf")),
            ContentKind::File
        );
    }
}
