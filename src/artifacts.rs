use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use url::Url;

use crate::context::ensure_private_dir;
use crate::extract::ResourceKind;

pub const MAX_ARTIFACT_CACHE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_STEM_LEN: usize = 40;
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StagedArtifact {
    pub path: PathBuf,
    pub content_type: Option<String>,
    pub byte_count: usize,
}

pub fn stage_artifact(
    state_dir: &Path,
    source_url: &Url,
    content_type: Option<&str>,
    bytes: &[u8],
    expected_kind: Option<ResourceKind>,
) -> Result<StagedArtifact> {
    if bytes.is_empty() {
        bail!("refusing to stage empty artifact from {source_url}");
    }

    let declared_mime = crate::extract::extract_mime(content_type);
    let url_ext = extract_url_extension(source_url);
    let magic = detect_magic(bytes);

    let expects_binary = matches!(
        expected_kind,
        Some(ResourceKind::Diagram | ResourceKind::Image | ResourceKind::Document)
    ) || matches!(
        url_ext.as_deref(),
        Some("png" | "jpg" | "jpeg" | "webp" | "gif" | "pdf")
    );
    let expects_dataset = expected_kind == Some(ResourceKind::Dataset);
    let mime_is_html = matches!(
        declared_mime.as_deref(),
        Some("text/html" | "application/xhtml+xml")
    );
    let mime_claims_binary = declared_mime
        .as_deref()
        .is_some_and(|m| m.starts_with("image/") || m == "application/pdf");

    if (expects_binary || expects_dataset) && mime_is_html {
        bail!(
            "refusing to stage HTML response ({}) as {} artifact for {source_url}",
            declared_mime.as_deref().unwrap_or("text/html"),
            if expects_dataset { "dataset" } else { "binary" }
        );
    }
    if (expects_binary || mime_claims_binary || expects_dataset) && looks_like_html(bytes) {
        bail!("refusing to stage disguised HTML payload for {source_url}");
    }

    verify_claimed_magic(declared_mime.as_deref(), magic, source_url)?;

    let ext = magic
        .map(|(ext, _)| ext)
        .or_else(|| declared_mime.as_deref().and_then(mime_to_ext))
        .or_else(|| url_ext.as_deref().and_then(safe_url_ext))
        .unwrap_or("bin");

    let resolved_mime = match (declared_mime, magic) {
        (Some(m), Some((_, magic_mime))) if m == "application/octet-stream" => {
            Some(magic_mime.to_string())
        }
        (Some(m), _) => Some(m),
        (None, Some((_, magic_mime))) => Some(magic_mime.to_string()),
        (None, None) => None,
    };

    let hash = fnv1a_64(source_url.as_str().as_bytes(), bytes);
    let stem = sanitize_stem(source_url);
    let artifacts_dir = state_dir.join("artifacts");
    ensure_private_dir(&artifacts_dir)?;

    let final_path = artifacts_dir.join(format!("{hash:016x}-{stem}.{ext}"));
    write_atomic_private(&artifacts_dir, &final_path, bytes)?;

    let _ =
        evict_artifacts_over_budget(&artifacts_dir, MAX_ARTIFACT_CACHE_BYTES, Some(&final_path));

    Ok(StagedArtifact {
        path: final_path,
        content_type: resolved_mime,
        byte_count: bytes.len(),
    })
}

pub fn evict_artifacts_over_budget(
    dir: &Path,
    max_bytes: u64,
    protected_path: Option<&Path>,
) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }

    let mut entries: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    let mut total_bytes = 0u64;

    for entry in std::fs::read_dir(dir)? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with(".tmp.") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let len = meta.len();
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        total_bytes = total_bytes.saturating_add(len);
        entries.push((path, len, modified));
    }

    if total_bytes <= max_bytes {
        return Ok(0);
    }

    entries.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));

    let mut evicted = 0usize;
    for (path, len, _) in entries {
        if total_bytes <= max_bytes {
            break;
        }
        if protected_path.is_some_and(|p| p == path) {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            total_bytes = total_bytes.saturating_sub(len);
            evicted += 1;
        }
    }

    Ok(evicted)
}

fn write_atomic_private(dir: &Path, final_path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = dir.join(format!(".tmp.{}-{seq}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }

    let write_res = (|| -> Result<()> {
        let mut file = opts.open(&tmp_path)?;
        file.write_all(bytes)?;
        file.flush()?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Err(err) = std::fs::rename(&tmp_path, final_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err.into());
    }

    Ok(())
}

fn verify_claimed_magic(
    declared_mime: Option<&str>,
    magic: Option<(&str, &str)>,
    source_url: &Url,
) -> Result<()> {
    let expected = match declared_mime {
        Some("image/png") => Some("png"),
        Some("image/jpeg" | "image/jpg") => Some("jpg"),
        Some("image/gif") => Some("gif"),
        Some("image/webp") => Some("webp"),
        Some("application/pdf") => Some("pdf"),
        _ => None,
    };
    if let Some(exp) = expected {
        if magic.map(|(ext, _)| ext) != Some(exp) {
            bail!(
                "payload magic bytes do not match declared content-type {} for {source_url}",
                declared_mime.unwrap_or("unknown")
            );
        }
    }
    Ok(())
}

fn detect_magic(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("png", "image/png"))
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(("jpg", "image/jpeg"))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(("gif", "image/gif"))
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(("webp", "image/webp"))
    } else if bytes.starts_with(b"%PDF-") {
        Some(("pdf", "application/pdf"))
    } else {
        None
    }
}

fn looks_like_html(bytes: &[u8]) -> bool {
    let slice = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let trimmed = slice
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map(|i| &slice[i..])
        .unwrap_or(&[]);
    let head = &trimmed[..trimmed.len().min(32)];
    let lower: Vec<u8> = head.iter().map(u8::to_ascii_lowercase).collect();
    lower.starts_with(b"<!doctype html") || lower.starts_with(b"<html")
}

fn mime_to_ext(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/svg+xml" => Some("svg"),
        "image/avif" => Some("avif"),
        "application/pdf" => Some("pdf"),
        "text/vtt" => Some("vtt"),
        "application/x-subrip" | "application/srt" => Some("srt"),
        "text/csv" => Some("csv"),
        "text/tab-separated-values" => Some("tsv"),
        "application/json" | "application/schema+json" => Some("json"),
        "application/jsonl" | "application/x-ndjson" => Some("jsonl"),
        "application/yaml" | "text/yaml" | "application/x-yaml" => Some("yaml"),
        "application/vnd.apache.parquet" => Some("parquet"),
        "text/markdown" => Some("md"),
        "text/plain" => Some("txt"),
        "text/html" | "application/xhtml+xml" => Some("html"),
        "application/xml" | "text/xml" => Some("xml"),
        "application/sql" | "text/sql" => Some("sql"),
        m if m.starts_with("application/") && m.ends_with("+json") => Some("json"),
        _ => None,
    }
}

fn extract_url_extension(url: &Url) -> Option<String> {
    let segment = url.path_segments()?.rfind(|s| !s.is_empty())?;
    let (_, ext) = segment.rsplit_once('.')?;
    if ext.is_empty() {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

fn safe_url_ext(ext: &str) -> Option<&'static str> {
    match ext {
        "png" => Some("png"),
        "jpg" | "jpeg" => Some("jpg"),
        "gif" => Some("gif"),
        "webp" => Some("webp"),
        "svg" => Some("svg"),
        "avif" => Some("avif"),
        "pdf" => Some("pdf"),
        "vtt" => Some("vtt"),
        "srt" => Some("srt"),
        "csv" => Some("csv"),
        "tsv" => Some("tsv"),
        "parquet" => Some("parquet"),
        "jsonl" => Some("jsonl"),
        "json" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "proto" => Some("proto"),
        "sql" => Some("sql"),
        "txt" => Some("txt"),
        "md" => Some("md"),
        "xml" => Some("xml"),
        _ => None,
    }
}

fn sanitize_stem(url: &Url) -> String {
    let segment = url
        .path_segments()
        .and_then(|mut segs| segs.rfind(|s| !s.is_empty()))
        .unwrap_or("");
    let base = match segment.rsplit_once('.') {
        Some((prefix, _)) if !prefix.is_empty() => prefix,
        _ => segment,
    };
    let clean: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(MAX_STEM_LEN)
        .collect();
    if clean.is_empty() {
        "resource".to_string()
    } else {
        clean
    }
}

fn fnv1a_64(url_bytes: &[u8], payload: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &b in url_bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash ^= 0u64;
    hash = hash.wrapping_mul(0x100000001b3);
    for &b in payload {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/pleb-t-{}-{tag}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn stage_artifact_validates_magic_sanitizes_stem_and_rejects_disguised_html() {
        let dir = temp_dir("art");
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        let url =
            Url::parse("https://cdn.example.com/../../etc/passwd/arch%20diagram!.png").unwrap();
        let staged = stage_artifact(
            &dir,
            &url,
            Some("image/png"),
            png,
            Some(ResourceKind::Diagram),
        )
        .unwrap();
        assert_eq!(staged.byte_count, png.len());
        assert_eq!(staged.content_type.as_deref(), Some("image/png"));
        let fname = staged.path.file_name().unwrap().to_str().unwrap();
        assert!(fname.ends_with("-arch20diagram.png"), "got {fname}");
        assert_eq!(staged.path.parent().unwrap(), dir.join("artifacts"));
        assert_eq!(std::fs::read(&staged.path).unwrap(), png);

        // Disguised HTML under .png rejected.
        let html = b"  <!DOCTYPE html><html><body>Not found</body></html>";
        let err = stage_artifact(
            &dir,
            &url,
            Some("image/png"),
            html,
            Some(ResourceKind::Diagram),
        )
        .unwrap_err();
        assert!(err.to_string().contains("disguised HTML"));

        // HTML MIME for expected image rejected.
        let err2 = stage_artifact(&dir, &url, Some("text/html"), b"error", None).unwrap_err();
        assert!(err2.to_string().contains("HTML response"));

        // Bad magic for claimed image/png rejected.
        let err3 = stage_artifact(&dir, &url, Some("image/png"), b"not-a-png", None).unwrap_err();
        assert!(err3.to_string().contains("magic bytes"));

        // Empty payload rejected.
        assert!(stage_artifact(&dir, &url, Some("image/png"), b"", None).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evict_artifacts_over_budget_removes_oldest_and_respects_protected_path() {
        let dir = temp_dir("lru");
        let artifacts = dir.join("artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();

        let f1 = artifacts.join("0001-old.bin");
        let f2 = artifacts.join("0002-mid.bin");
        let f3 = artifacts.join("0003-new.bin");
        std::fs::write(&f1, vec![1u8; 100]).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&f2, vec![2u8; 100]).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&f3, vec![3u8; 100]).unwrap();

        // Budget 150 bytes with f1 protected -> f2 evicted first, f3 evicted next, f1 stays.
        let evicted = evict_artifacts_over_budget(&artifacts, 150, Some(&f1)).unwrap();
        assert_eq!(evicted, 2);
        assert!(f1.exists());
        assert!(!f2.exists());
        assert!(!f3.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
