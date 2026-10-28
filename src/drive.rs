//! Google Drive files and Colab notebooks.

use crate::{
    config,
    error::{Error, Result},
    google::{Api, status_error},
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const NOTEBOOK_MIME: &str = "application/vnd.google.colaboratory";
const FIELDS: &str = "files(id,name,mimeType,size,modifiedTime,parents,webViewLink),nextPageToken";

fn api(path: &str) -> String {
    format!("{}/{path}", config::google_api_url())
}

pub fn check_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 200
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::input(
            "Drive file IDs contain only letters, digits, '-' and '_'.",
        ));
    }
    Ok(())
}

/// Quote a value inside a Drive query string literal.
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

pub async fn search(
    api_: &Api,
    query: &str,
    page_size: u32,
    page_token: Option<&str>,
) -> Result<Value> {
    let page_size = page_size.clamp(1, 1000).to_string();
    let body = api_
        .json("Drive search", |http, token| {
            let mut request = http.get(api("drive/v3/files")).bearer_auth(token).query(&[
                ("q", query),
                ("orderBy", "modifiedTime desc"),
                ("pageSize", page_size.as_str()),
                ("fields", FIELDS),
            ]);
            if let Some(page) = page_token {
                request = request.query(&[("pageToken", page)]);
            }
            request
        })
        .await?;
    Ok(json!({"files": body["files"], "next_page": body["nextPageToken"]}))
}

pub async fn notebooks(
    api_: &Api,
    name_contains: Option<&str>,
    page_size: u32,
    page_token: Option<&str>,
) -> Result<Value> {
    let mut query = format!("mimeType={} and trashed=false", quote(NOTEBOOK_MIME));
    if let Some(text) = name_contains {
        query.push_str(&format!(" and name contains {}", quote(text)));
    }
    search(api_, &query, page_size, page_token).await
}

pub async fn metadata(api_: &Api, id: &str) -> Result<Value> {
    check_id(id)?;
    api_.json("Drive file", |http, token| {
        http.get(api(&format!("drive/v3/files/{id}")))
            .bearer_auth(token)
            .query(&[(
                "fields",
                "id,name,mimeType,size,modifiedTime,webViewLink,capabilities(canEdit)",
            )])
    })
    .await
}

/// Download a file's bytes, naming the local file after the Drive file by default.
pub async fn download(api_: &Api, id: &str, out: Option<&Path>, force: bool) -> Result<Value> {
    let meta = metadata(api_, id).await?;
    let name = meta["name"].as_str().unwrap_or("download.bin");
    if meta["mimeType"]
        .as_str()
        .is_some_and(|m| m.starts_with("application/vnd.google-apps."))
    {
        return Err(Error::new(
            "unsupported_file",
            format!("{name} is a Google Docs/Sheets/Slides file without downloadable bytes."),
            "Export it from Drive in the format you need.",
        ));
    }
    let safe_name: String = name
        .chars()
        .map(|c| if c == '/' || c.is_control() { '_' } else { c })
        .collect();
    let out: PathBuf = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(&safe_name));
    let response = api_
        .send("Drive download", |http, token| {
            http.get(api(&format!("drive/v3/files/{id}")))
                .bearer_auth(token)
                .query(&[("alt", "media")])
        })
        .await?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(status_error(status, "Drive download"));
    }
    let bytes = crate::kernel::write_stream(response, &out, force).await?;
    Ok(
        json!({"saved": out, "bytes": bytes, "name": name, "media_type": meta["mimeType"], "id": id}),
    )
}

pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

/// Find or create a folder path such as `results/run-1` under My Drive.
pub async fn ensure_folder(api_: &Api, path: &str) -> Result<Value> {
    ensure_folder_in(api_, "root", path).await
}

/// Find or create a folder path under the folder `start` (an ID or "root").
pub async fn ensure_folder_in(api_: &Api, start: &str, path: &str) -> Result<Value> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() || parts.len() > 20 || parts.iter().any(|p| p.len() > 255) {
        return Err(Error::input(
            "Folder paths look like results or results/run-1.",
        ));
    }
    let mut parent = start.to_owned();
    let mut created = false;
    for name in parts {
        let query = format!(
            "name = {} and mimeType = {} and {} in parents and trashed = false",
            quote(name),
            quote(FOLDER_MIME),
            quote(&parent)
        );
        let found = search(api_, &query, 1, None).await?;
        parent = match found["files"][0]["id"].as_str() {
            Some(id) => id.to_owned(),
            None => {
                created = true;
                let folder = api_
                    .json("Folder create", |http, token| {
                        http.post(api("drive/v3/files"))
                            .bearer_auth(token)
                            .query(&[("fields", "id")])
                            .json(&json!({"name": name, "mimeType": FOLDER_MIME, "parents": [parent]}))
                    })
                    .await?;
                folder["id"]
                    .as_str()
                    .ok_or_else(|| Error::network("Folder create"))?
                    .to_owned()
            }
        };
    }
    Ok(json!({"id": parent, "path": path, "created": created}))
}

/// Destination folder ID from an explicit ID or a folder path (created if missing).
pub async fn destination(
    api_: &Api,
    parent: Option<&str>,
    folder: Option<&str>,
) -> Result<Option<String>> {
    match (parent, folder) {
        (Some(_), Some(_)) => Err(Error::input("Give a folder path or a parent ID, not both.")),
        (Some(parent), None) => {
            check_id(parent)?;
            Ok(Some(parent.to_owned()))
        }
        (None, Some(folder)) => Ok(Some(
            ensure_folder(api_, folder).await?["id"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        )),
        (None, None) => Ok(None),
    }
}

/// Upload a local file (resumable, streamed).
pub async fn upload(
    api_: &Api,
    local: &Path,
    name: Option<&str>,
    parent: Option<&str>,
    mime: Option<&str>,
) -> Result<Value> {
    let size = tokio::fs::metadata(local).await?.len();
    let name = name
        .map(str::to_owned)
        .or_else(|| local.file_name().map(|n| n.to_string_lossy().to_string()))
        .ok_or_else(|| Error::input("Give the upload a --name."))?;
    let file = tokio::fs::File::open(local).await?;
    upload_body(
        api_,
        &name,
        parent,
        mime,
        size,
        reqwest::Body::wrap_stream(tokio_util_stream(file)),
    )
    .await
}

/// Resumable upload: metadata first, then the body streamed in one request.
pub async fn upload_body(
    api_: &Api,
    name: &str,
    parent: Option<&str>,
    mime: Option<&str>,
    size: u64,
    body: reqwest::Body,
) -> Result<Value> {
    let mut metadata = json!({"name": name});
    if let Some(parent) = parent {
        check_id(parent)?;
        metadata["parents"] = json!([parent]);
    }
    // Notebooks open in Colab only with Colab's MIME type.
    let mime = mime.or(name.ends_with(".ipynb").then_some(NOTEBOOK_MIME));
    if let Some(mime) = mime {
        metadata["mimeType"] = json!(mime);
    }
    let start = api_
        .send("Drive upload", |http, token| {
            http.post(api("upload/drive/v3/files"))
                .bearer_auth(token)
                .query(&[
                    ("uploadType", "resumable"),
                    ("fields", "id,name,mimeType,size,parents,webViewLink"),
                ])
                .header("x-upload-content-length", size.to_string())
                .json(&metadata)
        })
        .await?;
    if !start.status().is_success() {
        return Err(status_error(start.status().as_u16(), "Drive upload"));
    }
    let session = start
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .filter(|l| l.starts_with(&config::google_api_url()))
        .ok_or_else(|| Error::network("Drive upload"))?
        .to_owned();
    let token = api_.auth.access_token().await?;
    let response = api_
        .http
        .put(&session)
        .bearer_auth(token)
        .header("content-length", size.to_string())
        .body(body)
        .send()
        .await
        .map_err(|_| Error::outcome_unknown("Drive upload"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(status_error(status, "Drive upload"));
    }
    let bytes = crate::google::read_limited(response, 1024 * 1024, "Drive upload").await?;
    crate::google::parse_json(&bytes)
}

/// A file as a byte stream without loading it into memory.
fn tokio_util_stream(
    file: tokio::fs::File,
) -> impl futures_util::Stream<Item = std::io::Result<Vec<u8>>> {
    futures_util::stream::unfold(file, |mut file| async move {
        use tokio::io::AsyncReadExt;
        let mut buffer = vec![0u8; 1024 * 1024];
        match file.read(&mut buffer).await {
            Ok(0) => None,
            Ok(read) => {
                buffer.truncate(read);
                Some((Ok(buffer), file))
            }
            Err(error) => Some((Err(error), file)),
        }
    })
}

pub async fn read_notebook(api_: &Api, id: &str) -> Result<Value> {
    check_id(id)?;
    api_.json("Notebook read", |http, token| {
        http.get(api(&format!("drive/v3/files/{id}")))
            .bearer_auth(token)
            .query(&[("alt", "media")])
    })
    .await
}

/// Compact view for agents: each cell's type and source, outputs as text.
pub fn summarize_notebook(notebook: &Value) -> Value {
    let cells: Vec<Value> = notebook["cells"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, cell)| {
            let joined = |value: &Value| match value {
                Value::Array(lines) => lines.iter().filter_map(Value::as_str).collect::<String>(),
                Value::String(text) => text.clone(),
                _ => String::new(),
            };
            let mut text_output = String::new();
            for output in cell["outputs"].as_array().into_iter().flatten() {
                text_output.push_str(&joined(&output["text"]));
                text_output.push_str(&joined(&output["data"]["text/plain"]));
                if output["output_type"] == "error" {
                    text_output.push_str(&format!("{}: {}", output["ename"].as_str().unwrap_or(""), output["evalue"].as_str().unwrap_or("")));
                }
            }
            json!({"index": index, "type": cell["cell_type"], "source": joined(&cell["source"]), "output": crate::kernel::clip(&text_output).0})
        })
        .collect();
    json!({"cells": cells, "cell_count": cells.len()})
}

pub async fn create_notebook(api_: &Api, name: &str, parent: Option<&str>) -> Result<Value> {
    let notebook = json!({"cells": [], "metadata": {"colab": {"name": name, "provenance": []},
        "kernelspec": {"name": "python3", "display_name": "Python 3"}}, "nbformat": 4, "nbformat_minor": 0});
    let mut metadata = json!({"name": name, "mimeType": NOTEBOOK_MIME});
    if let Some(parent) = parent {
        check_id(parent)?;
        metadata["parents"] = json!([parent]);
    }
    let boundary = format!("colab-cli-{}", uuid::Uuid::new_v4().simple());
    let body = format!(
        "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{metadata}\r\n--{boundary}\r\nContent-Type: {NOTEBOOK_MIME}\r\n\r\n{notebook}\r\n--{boundary}--\r\n"
    );
    api_.json("Notebook create", |http, token| {
        http.post(api("upload/drive/v3/files"))
            .bearer_auth(token)
            .query(&[
                ("uploadType", "multipart"),
                ("fields", "id,name,webViewLink"),
            ])
            .header(
                "content-type",
                format!("multipart/related; boundary={boundary}"),
            )
            .body(body.clone())
    })
    .await
}

pub async fn save_notebook(api_: &Api, id: &str, notebook: &Value) -> Result<Value> {
    check_id(id)?;
    let body = serde_json::to_vec(notebook)?;
    api_.json("Notebook save", |http, token| {
        http.patch(api(&format!("upload/drive/v3/files/{id}")))
            .bearer_auth(token)
            .query(&[
                ("uploadType", "media"),
                ("fields", "id,name,modifiedTime,webViewLink"),
            ])
            .header("content-type", NOTEBOOK_MIME)
            .body(body.clone())
    })
    .await
}

pub async fn copy(api_: &Api, id: &str, name: &str) -> Result<Value> {
    check_id(id)?;
    api_.json("Notebook copy", |http, token| {
        http.post(api(&format!("drive/v3/files/{id}/copy")))
            .bearer_auth(token)
            .query(&[("fields", "id,name,webViewLink")])
            .json(&json!({"name": name}))
    })
    .await
}

pub async fn rename(api_: &Api, id: &str, name: &str) -> Result<Value> {
    check_id(id)?;
    api_.json("Rename", |http, token| {
        http.patch(api(&format!("drive/v3/files/{id}")))
            .bearer_auth(token)
            .query(&[("fields", "id,name,webViewLink")])
            .json(&json!({"name": name}))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_and_ids_are_safe() {
        assert_eq!(quote("it's"), "'it\\'s'");
        assert!(check_id("1aHcRRblAyE9U8PXcAJnkJRWP_21uL2a6").is_ok());
        assert!(check_id("../x").is_err());
    }

    #[test]
    fn notebook_summary_is_compact() {
        let summary = summarize_notebook(&json!({"cells": [
            {"cell_type": "code", "source": ["x = 1\n", "x"], "outputs": [{"output_type": "execute_result", "data": {"text/plain": ["1"]}}]},
            {"cell_type": "markdown", "source": "# Title"}
        ]}));
        assert_eq!(summary["cell_count"], 2);
        assert_eq!(summary["cells"][0]["source"], "x = 1\nx");
        assert_eq!(summary["cells"][0]["output"], "1");
        assert_eq!(summary["cells"][1]["type"], "markdown");
    }
}
