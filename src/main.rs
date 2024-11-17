use axum::{
    extract::{DefaultBodyLimit, Multipart},
    extract::{Path, State},
    http::StatusCode,
    response::Html,
    routing::{get, post, delete},
    Json, Router,
};
use base64::{display::Base64Display, engine::general_purpose::STANDARD};
use chrono::Local;
use clap::Parser;
use comrak::{markdown_to_html, Options};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self},
    io::Write,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::process::Command;
use tokio::spawn;
use tower_http::services::ServeDir;
use tracing::{error, info};
use tracing_subscriber;

const INDEX_HTML: &str = include_str!("index.html");
const FAVICON_SVG: &[u8] = include_bytes!("favicon.svg");

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Port number for the server
    #[arg(short, long, default_value_t = 3000)]
    port: u16,
    /// Listen address for the server
    #[arg(short, long, default_value_t = String::from("127.0.0.1"))]
    listen: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Note {
    timestamp: String,
    content: String,
    html: String,
}

#[derive(Clone)]
struct AppState {
    html: String,
    notes: Arc<Mutex<Vec<Note>>>,
}

const CONTENT_LENGTH_LIMIT: usize = 500 * 1024 * 1024; // allow uploading up to 500mb files... overkill?

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    fs::create_dir_all("attachments").unwrap();

    let favicon = Base64Display::new(FAVICON_SVG, &STANDARD);
    let html = INDEX_HTML.replace(
        "{{FAVICON}}",
        format!("data:image/svg+xml;base64,{favicon}").as_str(),
    );

    let state = AppState {
        html,
        notes: Arc::new(Mutex::new(load_notes())),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/save", post(save_note))
        .route("/search/:query", get(search_notes))
        .route("/upload", post(upload_file))
        .route("/delete/:timestamp", delete(delete_note))
        .layer(DefaultBodyLimit::max(CONTENT_LENGTH_LIMIT))
        .nest_service("/attachments", ServeDir::new("attachments"))
        .with_state(state);

    let server_details = format!("{}:{}", args.listen, args.port);
    let addr: SocketAddr = server_details
        .parse()
        .expect("Unable to parse socket address");
    println!("Starting server on http://{}", addr);

    match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => {
            if let Err(e) = axum::serve(listener, app).await {
                error!("Server error: {}", e);
            }
        }
        Err(e) => {
            error!("Failed to bind to address {}: {}", addr, e);
        }
    }
}

fn load_notes() -> Vec<Note> {
    if let Ok(content) = fs::read_to_string("notes.md") {
        content
            .split("\n\n---\n\n")
            .filter(|s| !s.trim().is_empty())
            .map(|block| {
                let parts: Vec<&str> = block.splitn(2, '\n').collect();
                let (timestamp, content) = match parts.as_slice() {
                    [timestamp, content] => {
                        (timestamp.trim().to_string(), content.trim().to_string())
                    }
                    _ => (
                        Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                        block.to_string(),
                    ),
                };

                let html = md_to_html(&content);
                Note {
                    timestamp,
                    content: content.to_string(),
                    html,
                }
            })
            .collect()
    } else {
        Vec::new()
    }
}

// route / (root)
async fn index(State(state): State<AppState>) -> Html<String> {
    let notes = state.notes.lock().unwrap();
    let notes_html = notes
        .iter()
        .rev()
        .map(|note| {
            format!(
                "<div class=\"note\">{}<div class=\"noteMetadata\"><time datetime=\"{}\">{}</time> [<a href=\"#\" data-timestamp=\"{}\" onclick=\"deleteNote(event, this)\">delete</a>]</div></div>",
                note.html, note.timestamp, note.timestamp, note.timestamp
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let html = state.html.replace("{{NOTES}}", &notes_html);
    Html(html)
}

// route /save
async fn save_note(
    State(state): State<AppState>,
    Json(content): Json<String>,
) -> Result<(), StatusCode> {
    let mut content = content.clone();

    // Replace "---" with "<hr>" in the content
    content = content.replace("---", "<hr>");
    let links_to_download: Vec<String> = content
        .split_whitespace()
        .filter(|word| word.starts_with("+http"))
        .map(|s| s.to_string())
        .collect();

    fs::create_dir_all("attachments/webpages").unwrap();

    for link in &links_to_download {
        let url = &link[1..];
        let escaped_filename = url_to_safe_filename(url);
        let filepath = format!("attachments/webpages/{}.html", escaped_filename);
        content = content.replace(link, &format!("{} ([local copy](/{}))", url, filepath));
    }

    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let html = md_to_html(&content); // Changed to pass a reference
    let note = Note {
        timestamp: timestamp.clone(),
        content: content.clone(),
        html,
    };

    state.notes.lock().unwrap().push(note);

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("notes.md")
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    write!(file, "{}\n{}\n\n---\n\n", timestamp, content)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    info!("Note created: {}", timestamp);

    if !links_to_download.is_empty() {
        let notes = state.notes.clone();
        spawn(async move {
            for link in links_to_download {
                let url = &link[1..];
                let escaped_filename = url_to_safe_filename(url);
                let filepath = format!("attachments/webpages/{}.html", escaped_filename);

                let result = Command::new("monolith")
                    .args(&[url, "-o", &filepath])
                    .output()
                    .await;

                info!("Downloading webpage: {}", url);

                if result.is_err() {
                    error!("Failed to download webpage: {}", url);
                    let mut notes_lock = notes.lock().unwrap();
                    if let Some(last_note) = notes_lock.last_mut() {
                        let updated_content = last_note.content.replace(
                            &format!("([local copy](/{}))", filepath),
                            "(local copy failed)",
                        );
                        last_note.content = updated_content.clone();
                        last_note.html = md_to_html(&updated_content); // Changed to pass a reference here too

                        drop(notes_lock);

                        if let Ok(file_content) = fs::read_to_string("notes.md") {
                            let notes_lock = notes.lock().unwrap();
                            let updated_content: Vec<String> = file_content
                                .split("\n---\n")
                                .enumerate()
                                .map(|(i, note_content)| {
                                    if i == notes_lock.len() - 1 {
                                        format!("{}\n{}", timestamp, updated_content)
                                    } else {
                                        note_content.to_string()
                                    }
                                })
                                .collect();
                            drop(notes_lock);

                            if let Ok(mut file) = fs::File::create("notes.md") {
                                for note_content in updated_content {
                                    writeln!(file, "{}\n---", note_content).ok();
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    Ok(())
}

// route DELETE /delete/{timestamp}
async fn delete_note(
    State(state): State<AppState>,
    Path(timestamp): Path<String>,
) -> Result<(), StatusCode> {
    let mut notes = state.notes.lock().unwrap();

    // Find and remove the note with matching timestamp
    if let Some(index) = notes.iter().position(|note| note.timestamp == timestamp) {
        notes.remove(index);

        // Update the notes.md file
        let content = notes
            .iter()
            .map(|note| format!("{}\n{}\n\n---\n\n", note.timestamp, note.content))
            .collect::<String>();

        fs::write("notes.md", content).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        info!("Note deleted: {}", timestamp);
        Ok(())
    } else {
        error!("Note not found: {}", timestamp);
        Err(StatusCode::NOT_FOUND)
    }
}

// route GET /search/{query}
async fn search_notes(State(state): State<AppState>, Path(query): Path<String>) -> Json<Vec<Note>> {
    let notes = state.notes.lock().unwrap();
    let filtered: Vec<Note> = notes
        .iter()
        .filter(|note| note.content.to_lowercase().contains(&query.to_lowercase()))
        .cloned()
        .collect();
    Json(filtered)
}

// route POST /upload
async fn upload_file(mut multipart: Multipart) -> Result<Json<String>, StatusCode> {
    while let Some(field) = multipart.next_field().await.unwrap() {
        let name = field.file_name().unwrap().to_string();
        let data = field.bytes().await.unwrap();

        info!("Uploading file: {}", name);
        let path = PathBuf::from("attachments").join(&name);
        fs::write(path, data).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        return Ok(Json(format!("/attachments/{}", name)));
    }

    error!("Error uploading file");
    Err(StatusCode::BAD_REQUEST)
}

// UTILS
fn md_to_html(markdown: &str) -> String {
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.tagfilter = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    options.extension.superscript = true;
    options.render.unsafe_ = true;
    markdown_to_html(markdown, &options)
}

fn url_to_safe_filename(url: &str) -> String {
    let mut safe_name = String::with_capacity(url.len());

    let stripped_url = url
        .trim()
        .strip_prefix("http://")
        .unwrap_or(url)
        .strip_prefix("https://")
        .unwrap_or(url);

    for c in stripped_url.chars() {
        match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => safe_name.push('_'),
            c if c.is_alphanumeric() || c == '-' || c == '.' || c == '_' => safe_name.push(c),
            _ => safe_name.push('_'),
        }
    }

    safe_name.trim_matches(|c| c == '.' || c == ' ').to_string()
}
