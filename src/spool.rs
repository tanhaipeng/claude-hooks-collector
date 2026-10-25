use std::{
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use serde_json::Value;
use tokio::time::{MissedTickBehavior, interval};

use crate::util::{SPAN_COUNTER, now_ns};

pub(crate) async fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    #[cfg(unix)]
    tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await?;
    Ok(())
}

async fn secure_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

pub(crate) async fn spool_batch(spool_dir: &Path, body: &Value) -> Result<(), String> {
    ensure_private_dir(spool_dir)
        .await
        .map_err(|error| error.to_string())?;

    let sequence = SPAN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!("batch-{}-{sequence}.json", now_ns());
    let path = spool_dir.join(&name);
    let tmp_path = spool_dir.join(format!(".{name}.tmp"));

    tokio::fs::write(&tmp_path, body.to_string())
        .await
        .map_err(|error| error.to_string())?;
    secure_file(&tmp_path)
        .await
        .map_err(|error| error.to_string())?;
    if let Err(error) = tokio::fs::rename(&tmp_path, &path).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(error.to_string());
    }
    Ok(())
}

pub(crate) async fn replay_loop(
    client: reqwest::Client,
    endpoint: String,
    spool_dir: PathBuf,
    replay_seconds: u64,
) {
    if let Err(error) = replay_once(&client, &endpoint, &spool_dir).await {
        tracing::warn!(error = %error, "initial spool replay failed");
    }

    let mut ticker = interval(Duration::from_secs(replay_seconds.max(1)));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        if let Err(error) = replay_once(&client, &endpoint, &spool_dir).await {
            tracing::warn!(error = %error, "spool replay failed");
        }
    }
}

async fn replay_once(
    client: &reqwest::Client,
    endpoint: &str,
    spool_dir: &Path,
) -> Result<(), String> {
    let mut entries = match tokio::fs::read_dir(spool_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };

    let mut paths = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|error| error.to_string())?
    {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    paths.sort();

    for path in paths {
        let body = match tokio::fs::read_to_string(&path).await {
            Ok(body) => body,
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "cannot read spool file");
                continue;
            }
        };

        let response = client
            .post(endpoint)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await;

        match response {
            Ok(response) if response.status().is_success() => {
                if let Err(error) = tokio::fs::remove_file(&path).await {
                    tracing::warn!(path = %path.display(), error = %error, "cannot remove replayed spool file");
                } else {
                    tracing::info!(path = %path.display(), "replayed spool batch");
                }
            }
            Ok(response) if matches!(response.status().as_u16(), 400 | 413 | 415 | 422) => {
                let bad_path = path.with_extension(format!("bad-{}", now_ns()));
                if let Err(error) = tokio::fs::rename(&path, &bad_path).await {
                    tracing::warn!(path = %path.display(), error = %error, "cannot quarantine bad spool file");
                } else {
                    tracing::warn!(
                        path = %path.display(),
                        bad_path = %bad_path.display(),
                        status = %response.status(),
                        "quarantined bad spool file"
                    );
                }
            }
            Ok(response) => {
                tracing::warn!(
                    path = %path.display(),
                    status = %response.status(),
                    "spool replay rejected; will retry"
                );
                break;
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "spool replay failed");
                break;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn spools_with_private_permissions() {
        let dir =
            std::env::temp_dir().join(format!("claude-hooks-spool-test-{}", crate::util::now_ns()));
        spool_batch(&dir, &json!({"resourceSpans": []}))
            .await
            .unwrap();

        let file = std::fs::read_dir(&dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let contents = std::fs::read_to_string(&file).unwrap();
        assert!(contents.contains("resourceSpans"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let _ = std::fs::remove_dir_all(dir);
    }
}
